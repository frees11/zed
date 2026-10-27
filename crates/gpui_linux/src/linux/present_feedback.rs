use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use gpui::{PlatformPresentFeedback, PresentOutcome};

/// Present stamps older than this are rejected: they come from another clock, or arrived too
/// late to time a frame.
const MAX_PRESENT_AGE: Duration = Duration::from_secs(1);

/// A clock read together with [`Instant`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClockReading {
    pub(crate) clock_now: Duration,
    pub(crate) now: Instant,
}

impl ClockReading {
    pub(crate) fn read(clock_id: libc::clockid_t) -> Option<Self> {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let result = unsafe { libc::clock_gettime(clock_id, &mut time) };
        let now = Instant::now();
        if result != 0 {
            return None;
        }
        Some(Self {
            clock_now: Duration::new(
                u64::try_from(time.tv_sec).ok()?,
                u32::try_from(time.tv_nsec).ok()?,
            ),
            now,
        })
    }

    /// Moves `stamp`, a time on the read clock, onto [`Instant`]. Stamps in the future and
    /// stamps older than a second are rejected.
    pub(crate) fn instant_of(&self, stamp: Duration) -> Option<Instant> {
        let age = self.clock_now.checked_sub(stamp)?;
        if age > MAX_PRESENT_AGE {
            return None;
        }
        self.now.checked_sub(age)
    }
}

pub(crate) fn not_presented(draw_call: u64, at: Instant) -> PlatformPresentFeedback {
    PlatformPresentFeedback {
        draw_call,
        submitted: at,
        outcome: PresentOutcome::Unknown,
    }
}

/// The time of a `wp_presentation_feedback.presented` event, or `None` when its nanoseconds
/// are out of range.
#[cfg(feature = "wayland")]
pub(crate) fn presentation_stamp(tv_sec_hi: u32, tv_sec_lo: u32, tv_nsec: u32) -> Option<Duration> {
    if tv_nsec >= 1_000_000_000 {
        return None;
    }
    Some(Duration::new(
        (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo),
        tv_nsec,
    ))
}

/// The output's refresh counter of a `presented` event; zero means it has none.
#[cfg(feature = "wayland")]
pub(crate) fn presentation_sequence(seq_hi: u32, seq_lo: u32) -> Option<u64> {
    Some((u64::from(seq_hi) << 32) | u64::from(seq_lo)).filter(|sequence| *sequence != 0)
}

/// The refresh interval of a `presented` event; zero means it is unknown or variable.
#[cfg(feature = "wayland")]
pub(crate) fn presentation_refresh(refresh_nanoseconds: u32) -> Option<Duration> {
    (refresh_nanoseconds != 0).then(|| Duration::from_nanos(u64::from(refresh_nanoseconds)))
}

/// The presented draws of a Wayland window whose `wp_presentation_feedback` has not answered
/// yet. A draw that requested feedback but did not present is never recorded, so the answer to
/// its request, which the next commit without new content would get, is dropped.
#[cfg(feature = "wayland")]
#[derive(Default)]
pub(crate) struct PresentTickets {
    submitted: VecDeque<(u64, Instant)>,
}

#[cfg(feature = "wayland")]
impl PresentTickets {
    pub(crate) fn submitted(&mut self, draw_call: u64, at: Instant) {
        self.submitted.push_back((draw_call, at));
    }

    /// Answers `draw_call`'s request with `presented`; `at` is `None` when the compositor's
    /// time could not be moved onto [`Instant`].
    pub(crate) fn presented(
        &mut self,
        draw_call: u64,
        at: Option<Instant>,
        refresh: Option<Duration>,
        sequence: Option<u64>,
    ) -> Option<PlatformPresentFeedback> {
        let outcome = match at {
            Some(at) => PresentOutcome::Presented {
                at,
                refresh,
                sequence,
            },
            None => PresentOutcome::Unknown,
        };
        self.answer(draw_call, outcome)
    }

    pub(crate) fn discarded(&mut self, draw_call: u64) -> Option<PlatformPresentFeedback> {
        self.answer(draw_call, PresentOutcome::Discarded)
    }

    fn answer(
        &mut self,
        draw_call: u64,
        outcome: PresentOutcome,
    ) -> Option<PlatformPresentFeedback> {
        let index = self
            .submitted
            .iter()
            .position(|(submitted_draw_call, _)| *submitted_draw_call == draw_call)?;
        let (draw_call, submitted) = self.submitted.remove(index)?;
        Some(PlatformPresentFeedback {
            draw_call,
            submitted,
            outcome,
        })
    }
}

/// Presents that may wait for their `CompleteNotify` before a window stops expecting them: a
/// WSI that does not present through `PresentPixmap` never sends one.
#[cfg(feature = "x11")]
const PRESENTS_AWAITING_COMPLETE_NOTIFY: usize = 8;

/// Pairs an X11 window's presents with the Present extension's `CompleteNotify` events for
/// its pixmaps, oldest first.
#[cfg(feature = "x11")]
pub(crate) struct CompleteNotifyQueue {
    submitted: VecDeque<(u64, Instant)>,
    expecting: bool,
}

#[cfg(feature = "x11")]
impl Default for CompleteNotifyQueue {
    fn default() -> Self {
        Self {
            submitted: VecDeque::new(),
            expecting: true,
        }
    }
}

#[cfg(feature = "x11")]
impl CompleteNotifyQueue {
    /// Records a present and returns the feedback known already.
    pub(crate) fn submitted(
        &mut self,
        draw_call: u64,
        at: Instant,
    ) -> Vec<PlatformPresentFeedback> {
        if !self.expecting {
            return vec![not_presented(draw_call, at)];
        }
        self.submitted.push_back((draw_call, at));
        if self.submitted.len() < PRESENTS_AWAITING_COMPLETE_NOTIFY {
            return Vec::new();
        }
        self.expecting = false;
        self.submitted
            .drain(..)
            .map(|(draw_call, at)| not_presented(draw_call, at))
            .collect()
    }

    /// Answers the oldest present with a `CompleteNotify` event's fields. `ust` is the server's
    /// CLOCK_MONOTONIC in microseconds; `clock` is CLOCK_MONOTONIC read now.
    pub(crate) fn complete(
        &mut self,
        kind: x11rb::protocol::present::CompleteKind,
        mode: x11rb::protocol::present::CompleteMode,
        ust: u64,
        msc: u64,
        clock: Option<ClockReading>,
    ) -> Option<PlatformPresentFeedback> {
        use x11rb::protocol::present::{CompleteKind, CompleteMode};

        if kind != CompleteKind::PIXMAP || !self.expecting {
            return None;
        }
        let (draw_call, submitted) = self.submitted.pop_front()?;
        let outcome = if mode == CompleteMode::SKIP {
            PresentOutcome::Discarded
        } else {
            match clock.and_then(|clock| clock.instant_of(Duration::from_micros(ust))) {
                Some(at) => PresentOutcome::Presented {
                    at,
                    refresh: None,
                    sequence: Some(msc),
                },
                None => PresentOutcome::Unknown,
            }
        };
        Some(PlatformPresentFeedback {
            draw_call,
            submitted,
            outcome,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "x11")]
    use x11rb::protocol::present::{CompleteKind, CompleteMode};

    fn reading(clock_now: Duration) -> ClockReading {
        ClockReading {
            clock_now,
            now: Instant::now(),
        }
    }

    #[test]
    fn presented_times_move_from_the_presentation_clock_to_an_instant() {
        let clock = reading(Duration::from_secs(5_000));
        assert_eq!(
            clock.instant_of(Duration::from_secs(5_000) - Duration::from_millis(3)),
            Some(clock.now - Duration::from_millis(3))
        );
        assert_eq!(
            clock.instant_of(Duration::from_secs(5_000)),
            Some(clock.now)
        );
        let monotonic = ClockReading::read(libc::CLOCK_MONOTONIC).expect("CLOCK_MONOTONIC reads");
        let instant = monotonic
            .instant_of(monotonic.clock_now)
            .expect("a stamp of now is accepted");
        assert!(instant <= Instant::now());
    }

    #[test]
    fn stamps_in_the_future_or_older_than_a_second_are_rejected() {
        let clock = reading(Duration::from_secs(5_000));
        assert_eq!(
            clock.instant_of(Duration::from_secs(5_000) + Duration::from_nanos(1)),
            None
        );
        assert_eq!(
            clock.instant_of(Duration::from_secs(5_000) - Duration::from_millis(1_001)),
            None
        );
        assert!(clock.instant_of(Duration::from_secs(4_999)).is_some());
    }

    #[cfg(feature = "wayland")]
    #[test]
    fn discarded_feedback_reports_discarded() {
        let at = Instant::now();
        let mut tickets = PresentTickets::default();
        tickets.submitted(3, at);
        assert_eq!(
            tickets.discarded(3),
            Some(PlatformPresentFeedback {
                draw_call: 3,
                submitted: at,
                outcome: PresentOutcome::Discarded,
            })
        );
        assert_eq!(tickets.discarded(3), None, "a request is answered once");
    }

    #[cfg(feature = "wayland")]
    #[test]
    fn a_voided_ticket_reports_nothing_later() {
        let at = Instant::now();
        let mut tickets = PresentTickets::default();
        tickets.submitted(1, at);
        assert_eq!(tickets.presented(2, Some(at), None, None), None);
        assert_eq!(tickets.discarded(2), None);
        let presented = tickets
            .presented(1, Some(at), Some(Duration::from_millis(16)), Some(9))
            .expect("draw 1 presented");
        assert_eq!(
            presented.outcome,
            PresentOutcome::Presented {
                at,
                refresh: Some(Duration::from_millis(16)),
                sequence: Some(9),
            }
        );
        assert_eq!(
            tickets.presented(1, None, None, None),
            None,
            "a request is answered once"
        );

        tickets.submitted(4, at);
        assert_eq!(
            tickets
                .presented(4, None, None, None)
                .map(|feedback| feedback.outcome),
            Some(PresentOutcome::Unknown),
            "an untimed present is unknown"
        );
    }

    #[cfg(feature = "wayland")]
    #[test]
    fn the_hi_and_lo_halves_join_into_seconds_and_sequence() {
        assert_eq!(
            presentation_stamp(1, 2, 3),
            Some(Duration::new((1 << 32) + 2, 3))
        );
        assert_eq!(presentation_stamp(0, 7, 1_000_000_000), None);
        assert_eq!(presentation_sequence(2, 5), Some((2 << 32) + 5));
        assert_eq!(presentation_sequence(0, 0), None);
        assert_eq!(
            presentation_refresh(16_666_667),
            Some(Duration::from_nanos(16_666_667))
        );
        assert_eq!(presentation_refresh(0), None);
    }

    #[cfg(feature = "x11")]
    const UST_NOW: u64 = 7_000_000_000;

    #[cfg(feature = "x11")]
    fn clock_at_ust_now() -> Option<ClockReading> {
        Some(reading(Duration::from_micros(UST_NOW)))
    }

    #[cfg(feature = "x11")]
    #[test]
    fn complete_notifies_pop_the_oldest_draw() {
        let at = Instant::now();
        let mut queue = CompleteNotifyQueue::default();
        assert!(queue.submitted(1, at).is_empty());
        assert!(queue.submitted(2, at).is_empty());
        let clock = clock_at_ust_now();
        let first = queue
            .complete(
                CompleteKind::PIXMAP,
                CompleteMode::FLIP,
                UST_NOW - 2_000,
                41,
                clock,
            )
            .expect("draw 1 completed");
        assert_eq!(first.draw_call, 1);
        assert_eq!(
            first.outcome,
            PresentOutcome::Presented {
                at: clock
                    .map(|clock| clock.now - Duration::from_millis(2))
                    .expect("a clock"),
                refresh: None,
                sequence: Some(41),
            }
        );
        let second = queue
            .complete(CompleteKind::PIXMAP, CompleteMode::COPY, UST_NOW, 42, clock)
            .expect("draw 2 completed");
        assert_eq!(second.draw_call, 2);
        assert!(matches!(second.outcome, PresentOutcome::Presented { .. }));
        assert_eq!(
            queue.complete(CompleteKind::PIXMAP, CompleteMode::COPY, UST_NOW, 43, clock),
            None,
            "no present is left"
        );

        assert!(queue.submitted(3, at).is_empty());
        assert_eq!(
            queue
                .complete(
                    CompleteKind::PIXMAP,
                    CompleteMode::COPY,
                    UST_NOW + 1,
                    44,
                    clock
                )
                .map(|feedback| feedback.outcome),
            Some(PresentOutcome::Unknown),
            "a stamp in the future is not trusted"
        );
    }

    #[cfg(feature = "x11")]
    #[test]
    fn skipped_pixmaps_are_discarded() {
        let mut queue = CompleteNotifyQueue::default();
        assert!(queue.submitted(1, Instant::now()).is_empty());
        assert_eq!(
            queue
                .complete(
                    CompleteKind::PIXMAP,
                    CompleteMode::SKIP,
                    UST_NOW,
                    1,
                    clock_at_ust_now()
                )
                .map(|feedback| feedback.outcome),
            Some(PresentOutcome::Discarded)
        );
    }

    #[cfg(feature = "x11")]
    #[test]
    fn notify_msc_events_are_ignored() {
        let mut queue = CompleteNotifyQueue::default();
        assert!(queue.submitted(1, Instant::now()).is_empty());
        assert_eq!(
            queue.complete(
                CompleteKind::NOTIFY_MSC,
                CompleteMode::COPY,
                UST_NOW,
                1,
                clock_at_ust_now()
            ),
            None
        );
        assert_eq!(
            queue
                .complete(
                    CompleteKind::PIXMAP,
                    CompleteMode::COPY,
                    UST_NOW,
                    1,
                    clock_at_ust_now()
                )
                .map(|feedback| feedback.draw_call),
            Some(1),
            "the draw still waits for its pixmap"
        );
    }

    #[cfg(feature = "x11")]
    #[test]
    fn eight_presents_without_events_stop_the_expectation() {
        let at = Instant::now();
        let mut queue = CompleteNotifyQueue::default();
        for draw_call in 1..8 {
            assert!(queue.submitted(draw_call, at).is_empty());
        }
        let given_up = queue.submitted(8, at);
        assert_eq!(
            given_up
                .iter()
                .map(|feedback| (feedback.draw_call, feedback.outcome))
                .collect::<Vec<_>>(),
            (1..=8)
                .map(|draw_call| (draw_call, PresentOutcome::Unknown))
                .collect::<Vec<_>>()
        );
        assert_eq!(queue.submitted(9, at), [not_presented(9, at)]);
        assert_eq!(
            queue.complete(
                CompleteKind::PIXMAP,
                CompleteMode::COPY,
                UST_NOW,
                1,
                clock_at_ust_now()
            ),
            None,
            "late events are ignored once the window stopped expecting them"
        );
    }
}

/// Presents real frames through `LinuxPlatform`. Each test needs its display server: run one per
/// process, the Wayland test under a compositor with `DISPLAY` unset and the X11 test under an X
/// server with `WAYLAND_DISPLAY` unset, for example in `tools/linux/headless-session.sh` of the
/// Remote Ultra repository.
#[cfg(test)]
mod session {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
        time::{Duration, Instant},
    };

    use gpui::{
        AppContext as _, Application, Bounds, Context, FramePresented, IntoElement,
        ParentElement as _, PresentOutcome, QuitMode, Render, Styled as _, Window, WindowBounds,
        WindowOptions, WindowingModes, div, point, px, rgb, size,
    };

    const FRAMES: usize = 120;
    const SESSION_TIMEOUT: Duration = Duration::from_secs(30);
    const TRAILING_FEEDBACK_WAIT: Duration = Duration::from_millis(500);
    const POLL_INTERVAL: Duration = Duration::from_millis(20);
    const MAX_PRESENT_LATENCY: Duration = Duration::from_millis(250);

    /// Moves a quad on every frame until `FRAMES` frames were rendered.
    struct MovingQuad {
        frames: Rc<Cell<usize>>,
    }

    impl Render for MovingQuad {
        fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let frame = self.frames.get() + 1;
            self.frames.set(frame);
            if frame < FRAMES {
                window.request_animation_frame();
            }
            div().size_full().bg(rgb(0x000000)).child(
                div()
                    .absolute()
                    .left(px((frame % 48) as f32))
                    .top(px(8.))
                    .size(px(16.))
                    .bg(rgb(0xff0000)),
            )
        }
    }

    /// Opens a 64×64 window on the display server of the environment, renders `FRAMES` frames
    /// and returns the present feedback of their draws, or `None` when the platform reports
    /// none.
    fn presented_frames() -> Option<Vec<FramePresented>> {
        let reports = Rc::new(RefCell::new(Vec::new()));
        let supported = Rc::new(Cell::new(None));
        Application::with_platform(crate::linux_platform(WindowingModes::all()))
            .with_quit_mode(QuitMode::Explicit)
            .run({
                let reports = reports.clone();
                let supported = supported.clone();
                move |cx| {
                    let frames = Rc::new(Cell::new(0));
                    let window = cx
                        .open_window(
                            WindowOptions {
                                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                                    point(px(0.), px(0.)),
                                    size(px(64.), px(64.)),
                                ))),
                                ..WindowOptions::default()
                            },
                            {
                                let frames = frames.clone();
                                move |_, cx| cx.new(|_| MovingQuad { frames })
                            },
                        )
                        .expect("open a window");
                    let subscription = window
                        .update(cx, |_, window, _| {
                            window.on_frame_presented({
                                let reports = reports.clone();
                                move |presented| reports.borrow_mut().push(*presented)
                            })
                        })
                        .expect("the window exists");
                    supported.set(Some(subscription.is_some()));
                    cx.spawn(async move |cx| {
                        let deadline = Instant::now() + SESSION_TIMEOUT;
                        while frames.get() < FRAMES
                            && subscription.is_some()
                            && Instant::now() < deadline
                        {
                            cx.background_executor().timer(POLL_INTERVAL).await;
                        }
                        cx.background_executor().timer(TRAILING_FEEDBACK_WAIT).await;
                        drop(subscription);
                        cx.update(|cx| cx.quit());
                    })
                    .detach();
                }
            });
        let supported = supported.get().expect("the app ran");
        supported.then(|| reports.take())
    }

    /// The submit and present times of the reports that presented.
    fn presented_times(reports: &[FramePresented]) -> Vec<(Instant, Instant)> {
        reports
            .iter()
            .filter_map(|presented| match presented.outcome {
                PresentOutcome::Presented { at, .. } => Some((presented.submitted, at)),
                _ => None,
            })
            .collect()
    }

    /// How many reports presented, and their median and longest time from submit to present.
    fn latency_summary(reports: &[FramePresented], presented: &[(Instant, Instant)]) -> String {
        let mut latencies = presented
            .iter()
            .map(|(submitted, at)| at.saturating_duration_since(*submitted))
            .collect::<Vec<_>>();
        latencies.sort();
        format!(
            "{} of {} reports presented, submit to present median {:?}, longest {:?}",
            presented.len(),
            reports.len(),
            latencies.get(latencies.len() / 2),
            latencies.last()
        )
    }

    fn assert_presents_follow_their_submits(presented: &[(Instant, Instant)]) {
        for (submitted, at) in presented {
            assert!(
                at >= submitted,
                "shown {:?} before it was submitted",
                submitted.duration_since(*at)
            );
            assert!(
                at.duration_since(*submitted) < MAX_PRESENT_LATENCY,
                "shown {:?} after it was submitted",
                at.duration_since(*submitted)
            );
        }
        assert!(
            presented.windows(2).all(|pair| pair[0].1 < pair[1].1),
            "present times must increase: {presented:?}"
        );
    }

    #[cfg(feature = "wayland")]
    #[test]
    #[ignore = "compositor"]
    fn wayland_presents_report_a_time_after_submit() {
        assert!(
            std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_none(),
            "run under a Wayland compositor with DISPLAY unset"
        );
        let reports = presented_frames().expect("the compositor provides wp_presentation");
        let presented = presented_times(&reports);
        assert!(
            presented.len() * 10 >= FRAMES * 9,
            "only {} of {FRAMES} frames were presented: {reports:?}",
            presented.len()
        );
        assert_presents_follow_their_submits(&presented);
        eprintln!("wayland: {}", latency_summary(&reports, &presented));
    }

    #[cfg(feature = "x11")]
    #[test]
    #[ignore = "compositor"]
    fn x11_presents_report_a_time_after_submit_or_stop_expecting_them() {
        assert!(
            std::env::var_os("DISPLAY").is_some() && std::env::var_os("WAYLAND_DISPLAY").is_none(),
            "run under an X server with WAYLAND_DISPLAY unset"
        );
        let reports = presented_frames().expect("the X server has the Present extension");
        assert!(
            reports.len() * 10 >= FRAMES * 9,
            "only {} of {FRAMES} frames got feedback",
            reports.len()
        );
        let presented = presented_times(&reports);
        if presented.is_empty() {
            assert!(
                reports
                    .iter()
                    .all(|presented| presented.outcome == PresentOutcome::Unknown),
                "without CompleteNotify events every frame is unknown: {reports:?}"
            );
            eprintln!("x11: no CompleteNotify events; the window stopped expecting them");
        } else {
            assert_presents_follow_their_submits(&presented);
            eprintln!("x11: {}", latency_summary(&reports, &presented));
        }
    }
}
