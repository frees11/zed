//! The frame rate GPUI asks the display link for.

/// Displays refreshing this often or faster are ProMotion displays.
const PROMOTION_FRAMES_PER_SECOND: isize = 120;

/// The frames per second a display link may run at and the rate it prefers, as in
/// `CAFrameRateRange`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FrameRateRange {
    pub(crate) minimum: f32,
    pub(crate) maximum: f32,
    pub(crate) preferred: f32,
}

/// On ProMotion displays the display link asks for the full 120 Hz and never drops below
/// 80 Hz, so the next frame is at most 12.5 ms away. iPhones only exceed 60 Hz when the app's
/// Info.plist sets `CADisableMinimumFrameDurationOnPhone`.
const PROMOTION_FRAME_RATE_RANGE: FrameRateRange = FrameRateRange {
    minimum: 80.,
    maximum: 120.,
    preferred: 120.,
};

/// The range to request from the display link of a screen that refreshes at most
/// `maximum_frames_per_second` times a second, or none to keep the system's rate.
pub(crate) fn preferred_frame_rate_range(
    maximum_frames_per_second: isize,
) -> Option<FrameRateRange> {
    (maximum_frames_per_second >= PROMOTION_FRAMES_PER_SECOND).then_some(PROMOTION_FRAME_RATE_RANGE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The longest wait for a frame the range allows on a ProMotion display.
    const MAX_FRAME_INTERVAL_MS: f32 = 12.5;

    #[test]
    fn promotion_screens_ask_for_80_to_120_hz() {
        for maximum_frames_per_second in [120, 240] {
            assert_eq!(
                preferred_frame_rate_range(maximum_frames_per_second),
                Some(FrameRateRange {
                    minimum: 80.,
                    maximum: 120.,
                    preferred: 120.,
                }),
                "{maximum_frames_per_second} Hz"
            );
        }
    }

    #[test]
    fn screens_below_120_hz_keep_the_system_rate() {
        for maximum_frames_per_second in [0, 30, 60, 119] {
            assert_eq!(
                preferred_frame_rate_range(maximum_frames_per_second),
                None,
                "{maximum_frames_per_second} Hz"
            );
        }
    }

    #[test]
    fn the_promotion_range_is_valid_and_bounds_the_frame_interval() {
        let range = PROMOTION_FRAME_RATE_RANGE;
        assert!(range.minimum <= range.preferred && range.preferred <= range.maximum);
        assert_eq!(range.preferred, range.maximum);
        assert_eq!(range.maximum, PROMOTION_FRAMES_PER_SECOND as f32);
        assert!(1000. / range.minimum <= MAX_FRAME_INTERVAL_MS);
    }
}
