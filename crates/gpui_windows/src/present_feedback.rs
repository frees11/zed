use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use gpui::{PlatformPresentFeedback, PresentOutcome};
use windows::Win32::{
    Graphics::Dxgi::DXGI_FRAME_STATISTICS, System::Performance::QueryPerformanceCounter,
};

use crate::QPC_TICKS_PER_SECOND;

/// Present times older than this are rejected, and a present waits at most this long for
/// frame statistics that time it.
const MAX_PRESENT_AGE: Duration = Duration::from_secs(1);

/// The `DXGI_FRAME_STATISTICS` fields that time a present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameStatistics {
    /// The present ID of the last present that reached the screen.
    pub(crate) present_count: u32,
    /// The vsync count at which that present reached the screen.
    pub(crate) present_refresh_count: u32,
    /// The vsync count at `sync_qpc_time`.
    pub(crate) sync_refresh_count: u32,
    /// The performance counter at a recent vsync.
    pub(crate) sync_qpc_time: i64,
}

impl From<DXGI_FRAME_STATISTICS> for FrameStatistics {
    fn from(statistics: DXGI_FRAME_STATISTICS) -> Self {
        Self {
            present_count: statistics.PresentCount,
            present_refresh_count: statistics.PresentRefreshCount,
            sync_refresh_count: statistics.SyncRefreshCount,
            sync_qpc_time: statistics.SyncQPCTime,
        }
    }
}

/// The performance counter and [`Instant`] read together, to move counter values onto
/// `Instant`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct QpcClock {
    pub(crate) qpc: i64,
    pub(crate) now: Instant,
    pub(crate) frequency: u64,
}

impl QpcClock {
    pub(crate) fn now() -> windows::core::Result<Self> {
        let mut qpc = 0;
        unsafe { QueryPerformanceCounter(&mut qpc) }?;
        Ok(Self {
            qpc,
            now: Instant::now(),
            frequency: *QPC_TICKS_PER_SECOND,
        })
    }

    /// How long ago the counter read `qpc`, or `None` for a reading in the future.
    fn age_of(&self, qpc: i64) -> Option<Duration> {
        let ticks = u128::try_from(self.qpc.checked_sub(qpc)?).ok()?;
        let nanoseconds = ticks * 1_000_000_000 / u128::from(self.frequency.max(1));
        Some(Duration::from_nanos(u64::try_from(nanoseconds).ok()?))
    }
}

struct PendingPresent {
    draw_call: u64,
    present_count: u32,
    submitted: Instant,
}

/// Matches a swap chain's presents with its frame statistics. Statistics name only the last
/// present that reached the screen, so a present that a later one overtook before any sample
/// saw it is reported as [`PresentOutcome::Unknown`].
#[derive(Default)]
pub(crate) struct PresentTracker {
    pending: VecDeque<PendingPresent>,
    ready: Vec<PlatformPresentFeedback>,
}

impl PresentTracker {
    /// Records a successful present of `draw_call`, handed to the renderer `at`, whose present
    /// ID is `present_count` (`IDXGISwapChain::GetLastPresentCount` right after it).
    pub(crate) fn submitted(&mut self, draw_call: u64, present_count: u32, at: Instant) {
        self.pending.push_back(PendingPresent {
            draw_call,
            present_count,
            submitted: at,
        });
    }

    /// Records a draw, handed to the renderer `at`, that did not present.
    pub(crate) fn not_presented(&mut self, draw_call: u64, at: Instant) {
        self.ready.push(PlatformPresentFeedback {
            draw_call,
            submitted: at,
            outcome: PresentOutcome::Unknown,
        });
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Resolves pending presents from a frame statistics sample, `None` when the statistics
    /// are disjoint or unavailable. `refresh` is the vsync interval.
    pub(crate) fn sample(
        &mut self,
        statistics: Option<FrameStatistics>,
        clock: QpcClock,
        refresh: Option<Duration>,
    ) {
        if let Some(statistics) = statistics {
            while let Some(pending) = self.pending.front() {
                let presents_ahead = statistics
                    .present_count
                    .wrapping_sub(pending.present_count)
                    .cast_signed();
                if presents_ahead < 0 {
                    break;
                }
                let outcome = if presents_ahead == 0 {
                    presented_outcome(statistics, clock, refresh)
                } else {
                    PresentOutcome::Unknown
                };
                self.resolve_front(outcome);
            }
        }
        while self.pending.front().is_some_and(|pending| {
            clock.now.saturating_duration_since(pending.submitted) > MAX_PRESENT_AGE
        }) {
            self.resolve_front(PresentOutcome::Unknown);
        }
    }

    /// Ends every pending present as unknown, because the swap chain was recreated or resized.
    pub(crate) fn reset(&mut self) {
        while self.has_pending() {
            self.resolve_front(PresentOutcome::Unknown);
        }
    }

    pub(crate) fn take_ready(&mut self) -> Vec<PlatformPresentFeedback> {
        std::mem::take(&mut self.ready)
    }

    fn resolve_front(&mut self, outcome: PresentOutcome) {
        if let Some(pending) = self.pending.pop_front() {
            self.ready.push(PlatformPresentFeedback {
                draw_call: pending.draw_call,
                submitted: pending.submitted,
                outcome,
            });
        }
    }
}

/// The present the statistics name reached the screen `SyncRefreshCount - PresentRefreshCount`
/// vsyncs before the sampled vsync.
fn presented_outcome(
    statistics: FrameStatistics,
    clock: QpcClock,
    refresh: Option<Duration>,
) -> PresentOutcome {
    let presented_at = || {
        let refreshes_since_present = statistics
            .sync_refresh_count
            .wrapping_sub(statistics.present_refresh_count);
        if refreshes_since_present.cast_signed() < 0 {
            return None;
        }
        let before_sync = match refreshes_since_present {
            0 => Duration::ZERO,
            refreshes => refresh?.checked_mul(refreshes)?,
        };
        let age = clock
            .age_of(statistics.sync_qpc_time)?
            .checked_add(before_sync)?;
        if age > MAX_PRESENT_AGE {
            return None;
        }
        clock.now.checked_sub(age)
    };
    match presented_at() {
        Some(at) => PresentOutcome::Presented {
            at,
            refresh,
            sequence: Some(u64::from(statistics.present_refresh_count)),
        },
        None => PresentOutcome::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DirectXDevices, DirectXRenderer};
    use anyhow::{Context as _, Result};
    use gpui::{DevicePixels, Scene, Size, WindowBackgroundAppearance};
    use gpui_util::ResultExt as _;
    use windows::{
        Win32::{
            Foundation::{HWND, LPARAM, LRESULT, WPARAM},
            Graphics::Dwm::DwmFlush,
            System::LibraryLoader::GetModuleHandleW,
            UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE,
                PeekMessageW, RegisterClassW, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
                WS_POPUP, WS_VISIBLE,
            },
        },
        core::{PCWSTR, w},
    };

    const FREQUENCY: u64 = 10_000_000;
    const NOW_QPC: i64 = 50_000_000_000;
    const REFRESH: Duration = Duration::from_millis(10);

    fn clock(now: Instant) -> QpcClock {
        QpcClock {
            qpc: NOW_QPC,
            now,
            frequency: FREQUENCY,
        }
    }

    fn qpc_before_now(age: Duration) -> i64 {
        NOW_QPC
            - i64::try_from(age.as_nanos() * u128::from(FREQUENCY) / 1_000_000_000)
                .expect("test ages fit the counter")
    }

    fn statistics(
        present_count: u32,
        present_refresh: u32,
        sync_refresh: u32,
        sync_age: Duration,
    ) -> FrameStatistics {
        FrameStatistics {
            present_count,
            present_refresh_count: present_refresh,
            sync_refresh_count: sync_refresh,
            sync_qpc_time: qpc_before_now(sync_age),
        }
    }

    fn outcomes(tracker: &mut PresentTracker) -> Vec<(u64, PresentOutcome)> {
        tracker
            .take_ready()
            .into_iter()
            .map(|feedback| (feedback.draw_call, feedback.outcome))
            .collect()
    }

    #[test]
    fn a_present_is_reported_when_its_count_is_the_latest_presented() {
        let now = Instant::now();
        let submitted = now - Duration::from_millis(20);
        let mut tracker = PresentTracker::default();
        tracker.submitted(1, 7, submitted);
        tracker.sample(
            Some(statistics(6, 99, 99, Duration::from_millis(30))),
            clock(now),
            Some(REFRESH),
        );
        assert!(
            tracker.take_ready().is_empty(),
            "an older present is no news"
        );
        assert!(tracker.has_pending());

        tracker.sample(
            Some(statistics(7, 100, 100, Duration::from_millis(2))),
            clock(now),
            Some(REFRESH),
        );
        let ready = tracker.take_ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].draw_call, 1);
        assert_eq!(ready[0].submitted, submitted);
        assert_eq!(
            ready[0].outcome,
            PresentOutcome::Presented {
                at: now - Duration::from_millis(2),
                refresh: Some(REFRESH),
                sequence: Some(100),
            }
        );
        assert!(!tracker.has_pending());
    }

    #[test]
    fn the_vsync_time_moves_back_by_the_refreshes_between_present_and_sync() {
        let now = Instant::now();
        let mut tracker = PresentTracker::default();
        tracker.submitted(3, 40, now - Duration::from_millis(40));
        tracker.sample(
            Some(statistics(40, 98, 100, Duration::from_millis(1))),
            clock(now),
            Some(REFRESH),
        );
        assert_eq!(
            outcomes(&mut tracker),
            [(
                3,
                PresentOutcome::Presented {
                    at: now - Duration::from_millis(21),
                    refresh: Some(REFRESH),
                    sequence: Some(98),
                }
            )]
        );
    }

    #[test]
    fn a_vsync_behind_the_sync_without_a_refresh_interval_is_unknown() {
        let now = Instant::now();
        let mut tracker = PresentTracker::default();
        tracker.submitted(3, 40, now - Duration::from_millis(40));
        tracker.sample(
            Some(statistics(40, 98, 100, Duration::from_millis(1))),
            clock(now),
            None,
        );
        assert_eq!(outcomes(&mut tracker), [(3, PresentOutcome::Unknown)]);
    }

    #[test]
    fn presents_overtaken_by_a_newer_one_are_unknown() {
        let now = Instant::now();
        let mut tracker = PresentTracker::default();
        for (draw_call, present_count) in [(1, 5), (2, 6), (3, 7)] {
            tracker.submitted(draw_call, present_count, now - Duration::from_millis(30));
        }
        tracker.sample(
            Some(statistics(6, 50, 50, Duration::ZERO)),
            clock(now),
            Some(REFRESH),
        );
        assert_eq!(
            outcomes(&mut tracker),
            [
                (1, PresentOutcome::Unknown),
                (
                    2,
                    PresentOutcome::Presented {
                        at: now,
                        refresh: Some(REFRESH),
                        sequence: Some(50),
                    }
                ),
            ]
        );
        assert!(tracker.has_pending(), "the newest present still waits");
    }

    #[test]
    fn disjoint_statistics_keep_presents_pending() {
        let now = Instant::now();
        let mut tracker = PresentTracker::default();
        tracker.submitted(1, 1, now - Duration::from_millis(5));
        tracker.sample(None, clock(now), Some(REFRESH));
        assert!(tracker.take_ready().is_empty());
        assert!(tracker.has_pending());

        tracker.sample(
            Some(statistics(1, 10, 10, Duration::ZERO)),
            clock(now),
            Some(REFRESH),
        );
        assert!(matches!(
            outcomes(&mut tracker)[..],
            [(1, PresentOutcome::Presented { .. })]
        ));
    }

    #[test]
    fn presents_pending_for_more_than_a_second_are_unknown() {
        let now = Instant::now();
        let mut tracker = PresentTracker::default();
        tracker.submitted(1, 1, now - Duration::from_millis(1_001));
        tracker.submitted(2, 2, now - Duration::from_millis(999));
        tracker.sample(None, clock(now), Some(REFRESH));
        assert_eq!(outcomes(&mut tracker), [(1, PresentOutcome::Unknown)]);
        assert!(tracker.has_pending());
    }

    #[test]
    fn a_draw_without_a_present_is_unknown_at_once() {
        let at = Instant::now();
        let mut tracker = PresentTracker::default();
        tracker.not_presented(4, at);
        let ready = tracker.take_ready();
        assert_eq!(
            ready,
            [PlatformPresentFeedback {
                draw_call: 4,
                submitted: at,
                outcome: PresentOutcome::Unknown,
            }]
        );
        assert!(!tracker.has_pending());
    }

    #[test]
    fn qpc_times_in_the_future_or_older_than_a_second_are_rejected() {
        let now = Instant::now();
        let mut tracker = PresentTracker::default();
        tracker.submitted(1, 1, now - Duration::from_millis(5));
        let mut future = statistics(1, 10, 10, Duration::ZERO);
        future.sync_qpc_time = NOW_QPC + 1;
        tracker.sample(Some(future), clock(now), Some(REFRESH));
        assert_eq!(outcomes(&mut tracker), [(1, PresentOutcome::Unknown)]);

        tracker.submitted(2, 2, now - Duration::from_millis(5));
        tracker.sample(
            Some(statistics(2, 10, 10, Duration::from_millis(1_001))),
            clock(now),
            Some(REFRESH),
        );
        assert_eq!(outcomes(&mut tracker), [(2, PresentOutcome::Unknown)]);

        tracker.submitted(3, 3, now - Duration::from_millis(5));
        tracker.sample(
            Some(statistics(3, 0, 100, Duration::from_millis(1))),
            clock(now),
            Some(REFRESH),
        );
        assert_eq!(
            outcomes(&mut tracker),
            [(3, PresentOutcome::Unknown)],
            "100 vsyncs back is more than a second ago"
        );
    }

    #[test]
    fn present_counts_wrap_around() {
        let now = Instant::now();
        let mut tracker = PresentTracker::default();
        tracker.submitted(1, u32::MAX, now - Duration::from_millis(5));
        tracker.submitted(2, 0, now - Duration::from_millis(4));
        tracker.sample(
            Some(statistics(u32::MAX - 1, 9, 9, Duration::ZERO)),
            clock(now),
            Some(REFRESH),
        );
        assert!(tracker.take_ready().is_empty(), "both presents are newer");

        tracker.sample(
            Some(statistics(0, 10, 10, Duration::ZERO)),
            clock(now),
            Some(REFRESH),
        );
        assert!(matches!(
            outcomes(&mut tracker)[..],
            [
                (1, PresentOutcome::Unknown),
                (2, PresentOutcome::Presented { .. })
            ]
        ));

        let mut tracker = PresentTracker::default();
        tracker.submitted(3, 2, now);
        tracker.sample(
            Some(statistics(1, u32::MAX, 1, Duration::ZERO)),
            clock(now),
            Some(REFRESH),
        );
        assert!(tracker.take_ready().is_empty());
    }

    #[test]
    fn a_new_swap_chain_ends_every_pending_present_as_unknown() {
        let now = Instant::now();
        let mut tracker = PresentTracker::default();
        tracker.submitted(1, 1, now);
        tracker.submitted(2, 2, now);
        tracker.reset();
        assert_eq!(
            outcomes(&mut tracker),
            [(1, PresentOutcome::Unknown), (2, PresentOutcome::Unknown)]
        );
        assert!(!tracker.has_pending());
    }

    const WARP_FRAMES: usize = 120;
    const WARP_WINDOW_SIDE: i32 = 64;
    const WARP_WINDOW_CLASS: PCWSTR = w!("Zed::PresentFeedbackTest");

    /// A small visible popup that never takes focus, for a swap chain DWM composes.
    struct PopupWindow(HWND);

    impl PopupWindow {
        fn new() -> Result<Self> {
            let instance =
                unsafe { GetModuleHandleW(None) }.context("getting the module handle")?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(popup_window_procedure),
                hInstance: instance.into(),
                lpszClassName: WARP_WINDOW_CLASS,
                ..Default::default()
            };
            unsafe { RegisterClassW(&class) };
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    WARP_WINDOW_CLASS,
                    w!("present feedback"),
                    WS_POPUP | WS_VISIBLE,
                    0,
                    0,
                    WARP_WINDOW_SIDE,
                    WARP_WINDOW_SIDE,
                    None,
                    None,
                    Some(instance.into()),
                    None,
                )
            }
            .context("creating the popup window")?;
            Ok(Self(hwnd))
        }

        fn dispatch_messages(&self) {
            let mut message = MSG::default();
            while unsafe { PeekMessageW(&mut message, Some(self.0), 0, 0, PM_REMOVE) }.as_bool() {
                unsafe { DispatchMessageW(&message) };
            }
        }
    }

    impl Drop for PopupWindow {
        fn drop(&mut self) {
            unsafe { DestroyWindow(self.0) }
                .context("destroying the popup window")
                .log_err();
        }
    }

    unsafe extern "system" fn popup_window_procedure(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    /// Draws `WARP_FRAMES` frames paced by `DwmFlush` on WARP and returns every feedback.
    fn warp_present_feedback(
        disable_direct_composition: bool,
    ) -> Result<Vec<PlatformPresentFeedback>> {
        let window = PopupWindow::new()?;
        let devices = DirectXDevices::warp()?;
        let mut renderer = DirectXRenderer::new(window.0, &devices, disable_direct_composition)?;
        renderer.resize(Size {
            width: DevicePixels(WARP_WINDOW_SIDE),
            height: DevicePixels(WARP_WINDOW_SIDE),
        })?;
        renderer.enable_present_feedback();
        let scene = Scene::default();
        let mut feedback = Vec::new();
        for frame in 0..WARP_FRAMES {
            let background = if frame % 2 == 0 {
                WindowBackgroundAppearance::Opaque
            } else {
                WindowBackgroundAppearance::Transparent
            };
            renderer.draw(&scene, background)?;
            unsafe { DwmFlush() }.context("waiting for the compositor")?;
            window.dispatch_messages();
            feedback.extend(renderer.take_present_feedback());
        }
        let drain_until = Instant::now() + MAX_PRESENT_AGE * 2;
        while feedback.len() < WARP_FRAMES && Instant::now() < drain_until {
            unsafe { DwmFlush() }.context("waiting for the compositor")?;
            window.dispatch_messages();
            feedback.extend(renderer.take_present_feedback());
        }
        Ok(feedback)
    }

    #[test]
    #[ignore = "hw"]
    fn warp_presents_report_a_time_after_submit() -> Result<()> {
        for disable_direct_composition in [false, true] {
            let feedback = warp_present_feedback(disable_direct_composition)?;
            let mode = if disable_direct_composition {
                "without DirectComposition"
            } else {
                "with DirectComposition"
            };
            assert_eq!(feedback.len(), WARP_FRAMES, "{mode}: one feedback per draw");
            let presented = feedback
                .iter()
                .filter_map(|feedback| match feedback.outcome {
                    PresentOutcome::Presented { at, .. } => Some((feedback.submitted, at)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert!(
                presented.len() * 10 >= WARP_FRAMES * 9,
                "{mode}: only {} of {WARP_FRAMES} draws were timed: {feedback:?}",
                presented.len()
            );
            for (submitted, at) in &presented {
                assert!(at >= submitted, "{mode}: shown before it was submitted");
                assert!(
                    at.duration_since(*submitted) < Duration::from_millis(250),
                    "{mode}: shown {:?} after it was submitted",
                    at.duration_since(*submitted)
                );
            }
            assert!(
                presented.windows(2).all(|pair| pair[0].1 < pair[1].1),
                "{mode}: present times must increase: {presented:?}"
            );
        }
        Ok(())
    }
}
