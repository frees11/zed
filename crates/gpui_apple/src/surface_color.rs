//! How the Metal surface pipeline turns a `CVPixelBuffer`'s bi-planar 4:2:0 YCbCr samples into RGB.
//!
//! The code range comes from the pixel format (`420v` or `420f`) and the luma coefficients from the
//! buffer's `kCVImageBufferYCbCrMatrixKey` attachment, so video-range and BT.709 frames are not
//! decoded with the full-range BT.601 matrix the pipeline used to assume for every surface.

use anyhow::{Result, bail};
use objc2_core_foundation::CFType;
use objc2_core_video::{
    CVImageBuffer, CVPixelBufferGetPixelFormatType, kCVImageBufferYCbCrMatrix_ITU_R_601_4,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVImageBufferYCbCrMatrixKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use std::ptr;

/// The largest 8-bit code value, which a unorm texture samples as 1.0.
const CODE_MAX: f32 = 255.0;
/// The 8-bit chroma code of zero colour difference, in both ranges.
const CHROMA_ZERO_CODE: f32 = 128.0;

/// The luma coefficients a surface's YCbCr samples were encoded with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum YCbCrMatrix {
    /// ITU-R BT.601, also assumed for buffers without a YCbCr matrix attachment, as before.
    Bt601,
    /// ITU-R BT.709.
    Bt709,
}

impl YCbCrMatrix {
    /// The red and blue luma weights `(Kr, Kb)`; green is `1 - Kr - Kb`.
    fn red_and_blue_weights(self) -> (f32, f32) {
        match self {
            Self::Bt601 => (0.299, 0.114),
            Self::Bt709 => (0.2126, 0.0722),
        }
    }
}

/// The 8-bit code range of a surface's samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum YCbCrRange {
    /// `420v`: luma 16–235, chroma 16–240.
    Video,
    /// `420f`: luma and chroma 0–255.
    Full,
}

impl YCbCrRange {
    /// The luma code of black, the luma code span from black to white, and the chroma code span.
    fn code_spans(self) -> (f32, f32, f32) {
        match self {
            Self::Video => (16.0, 219.0, 224.0),
            Self::Full => (0.0, CODE_MAX, CODE_MAX),
        }
    }
}

/// How one surface's YCbCr samples map to RGB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SurfaceColor {
    pub matrix: YCbCrMatrix,
    pub range: YCbCrRange,
}

impl SurfaceColor {
    /// Reads the range from the buffer's pixel format and the matrix from its YCbCr matrix
    /// attachment; any other pixel format or matrix is an error, so the caller can skip the surface.
    pub(crate) fn of(image_buffer: &CVImageBuffer) -> Result<Self> {
        let matrix =
            unsafe { image_buffer.attachment(kCVImageBufferYCbCrMatrixKey, ptr::null_mut()) };
        Self::from_attachments(
            CVPixelBufferGetPixelFormatType(image_buffer),
            matrix.as_deref(),
        )
    }

    fn from_attachments(pixel_format: u32, matrix: Option<&CFType>) -> Result<Self> {
        let range = if pixel_format == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange {
            YCbCrRange::Video
        } else if pixel_format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange {
            YCbCrRange::Full
        } else {
            bail!("unsupported pixel format {pixel_format:#x}");
        };
        let (bt601, bt709): (&CFType, &CFType) = unsafe {
            (
                kCVImageBufferYCbCrMatrix_ITU_R_601_4.as_ref(),
                kCVImageBufferYCbCrMatrix_ITU_R_709_2.as_ref(),
            )
        };
        let matrix = match matrix {
            None => YCbCrMatrix::Bt601,
            Some(matrix) if matrix == bt601 => YCbCrMatrix::Bt601,
            Some(matrix) if matrix == bt709 => YCbCrMatrix::Bt709,
            Some(matrix) => bail!("unsupported YCbCr matrix {matrix:?}"),
        };
        Ok(Self { matrix, range })
    }

    /// The column-major matrix the surface fragment shader multiplies the sampled
    /// `(y, cb, cr, 1)` by to get `(r, g, b, 1)`.
    pub(crate) fn ycbcr_to_rgb(self) -> [[f32; 4]; 4] {
        let (red_weight, blue_weight) = self.matrix.red_and_blue_weights();
        let green_weight = 1.0 - red_weight - blue_weight;
        let (black_code, luma_span, chroma_span) = self.range.code_spans();

        let luma_scale = CODE_MAX / luma_span;
        let luma_offset = -black_code / luma_span;
        let chroma_scale = CODE_MAX / chroma_span;
        let chroma_offset = -CHROMA_ZERO_CODE / chroma_span;

        let cr_to_red = 2.0 * (1.0 - red_weight);
        let cb_to_blue = 2.0 * (1.0 - blue_weight);
        let cb_to_green = -cb_to_blue * blue_weight / green_weight;
        let cr_to_green = -cr_to_red * red_weight / green_weight;

        [
            [luma_scale, luma_scale, luma_scale, 0.0],
            [
                0.0,
                cb_to_green * chroma_scale,
                cb_to_blue * chroma_scale,
                0.0,
            ],
            [
                cr_to_red * chroma_scale,
                cr_to_green * chroma_scale,
                0.0,
                0.0,
            ],
            [
                luma_offset + cr_to_red * chroma_offset,
                luma_offset + (cb_to_green + cr_to_green) * chroma_offset,
                luma_offset + cb_to_blue * chroma_offset,
                1.0,
            ],
        ]
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use objc2_core_foundation::{CFNumber, CFString};
    use objc2_core_video::{
        kCVImageBufferYCbCrMatrix_ITU_R_2020, kCVImageBufferYCbCrMatrix_SMPTE_240M_1995,
        kCVPixelFormatType_32BGRA, kCVPixelFormatType_422YpCbCr8,
    };

    const VIDEO_601: SurfaceColor = SurfaceColor {
        matrix: YCbCrMatrix::Bt601,
        range: YCbCrRange::Video,
    };
    const VIDEO_709: SurfaceColor = SurfaceColor {
        matrix: YCbCrMatrix::Bt709,
        range: YCbCrRange::Video,
    };
    const FULL_601: SurfaceColor = SurfaceColor {
        matrix: YCbCrMatrix::Bt601,
        range: YCbCrRange::Full,
    };
    const FULL_709: SurfaceColor = SurfaceColor {
        matrix: YCbCrMatrix::Bt709,
        range: YCbCrRange::Full,
    };
    const ALL: [SurfaceColor; 4] = [VIDEO_601, VIDEO_709, FULL_601, FULL_709];

    /// One 8-bit code value of rounding in a sample is at most this far off once decoded.
    const DECODE_TOLERANCE: f32 = 1.5;

    pub(crate) fn decode(color: SurfaceColor, codes: [u8; 3]) -> [f32; 3] {
        let transform = color.ycbcr_to_rgb();
        let sample = [
            f32::from(codes[0]) / CODE_MAX,
            f32::from(codes[1]) / CODE_MAX,
            f32::from(codes[2]) / CODE_MAX,
            1.0,
        ];
        let mut rgb = [0.0; 3];
        for (channel, value) in rgb.iter_mut().enumerate() {
            *value = (0..4)
                .map(|column| transform[column][channel] * sample[column])
                .sum::<f32>()
                * CODE_MAX;
        }
        rgb
    }

    /// The textbook ITU forward conversion, written independently of the decoder's coefficients.
    pub(crate) fn encode(color: SurfaceColor, rgb: [u8; 3]) -> [u8; 3] {
        let (kr, kb) = match color.matrix {
            YCbCrMatrix::Bt601 => (0.299, 0.114),
            YCbCrMatrix::Bt709 => (0.2126, 0.0722),
        };
        let (black, luma_span, chroma_span) = match color.range {
            YCbCrRange::Video => (16.0, 219.0, 224.0),
            YCbCrRange::Full => (0.0, 255.0, 255.0),
        };
        let [r, g, b] = rgb.map(|channel| f32::from(channel) / 255.0);
        let y = kr * r + (1.0 - kr - kb) * g + kb * b;
        let pb = (b - y) / (2.0 * (1.0 - kb));
        let pr = (r - y) / (2.0 * (1.0 - kr));
        [
            black + luma_span * y,
            128.0 + chroma_span * pb,
            128.0 + chroma_span * pr,
        ]
        .map(|code: f32| code.round().clamp(0.0, 255.0) as u8)
    }

    fn assert_rgb_near(actual: [f32; 3], expected: [u8; 3], context: impl std::fmt::Debug) {
        for (actual, expected) in actual.iter().zip(expected) {
            assert!(
                (actual - f32::from(expected)).abs() <= DECODE_TOLERANCE,
                "{context:?}: decoded {actual:?}, expected {expected:?}"
            );
        }
    }

    fn cf(string: &CFString) -> &CFType {
        string.as_ref()
    }

    #[test]
    fn video_and_full_range_map_reference_black_and_white() {
        for color in [VIDEO_601, VIDEO_709] {
            assert_rgb_near(decode(color, [16, 128, 128]), [0, 0, 0], color);
            assert_rgb_near(decode(color, [235, 128, 128]), [255, 255, 255], color);
        }
        for color in [FULL_601, FULL_709] {
            assert_rgb_near(decode(color, [0, 128, 128]), [0, 0, 0], color);
            assert_rgb_near(decode(color, [255, 128, 128]), [255, 255, 255], color);
        }
    }

    #[test]
    fn video_range_decodes_published_colour_bar_codes() {
        let bars_709 = [
            ([63, 102, 240], [255, 0, 0]),
            ([173, 42, 26], [0, 255, 0]),
            ([32, 240, 118], [0, 0, 255]),
            ([219, 16, 138], [255, 255, 0]),
            ([188, 154, 16], [0, 255, 255]),
            ([78, 214, 230], [255, 0, 255]),
        ];
        let bars_601 = [
            ([81, 90, 240], [255, 0, 0]),
            ([145, 54, 34], [0, 255, 0]),
            ([41, 240, 110], [0, 0, 255]),
            ([210, 16, 146], [255, 255, 0]),
            ([170, 166, 16], [0, 255, 255]),
            ([106, 202, 222], [255, 0, 255]),
        ];
        for (color, bars) in [(VIDEO_709, bars_709), (VIDEO_601, bars_601)] {
            for (codes, rgb) in bars {
                let decoded = decode(color, codes).map(|channel| channel.clamp(0.0, CODE_MAX));
                assert_rgb_near(decoded, rgb, (color, codes));
            }
        }
    }

    #[test]
    fn transform_inverts_the_itu_forward_conversion() {
        let samples = [
            [200, 60, 30],
            [30, 140, 210],
            [128, 128, 128],
            [12, 240, 90],
            [250, 250, 5],
        ];
        for color in ALL {
            for rgb in samples {
                assert_rgb_near(decode(color, encode(color, rgb)), rgb, (color, rgb));
            }
        }
    }

    #[test]
    fn untagged_full_range_keeps_the_previous_bt601_coefficients() {
        let previous = [
            [1.0000, 1.0000, 1.0000, 0.0],
            [0.0000, -0.3441, 1.7720, 0.0],
            [1.4020, -0.7141, 0.0000, 0.0],
        ];
        let transform =
            SurfaceColor::from_attachments(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange, None)
                .unwrap()
                .ycbcr_to_rgb();
        for (column, previous) in transform.iter().zip(previous) {
            for (value, previous) in column.iter().zip(previous) {
                assert!((value - previous).abs() < 1e-4, "{transform:?}");
            }
        }
        assert_eq!(transform[3][3], 1.0);
    }

    #[test]
    fn range_follows_pixel_format_and_matrix_follows_attachment() {
        let (bt601, bt709) = unsafe {
            (
                cf(kCVImageBufferYCbCrMatrix_ITU_R_601_4),
                cf(kCVImageBufferYCbCrMatrix_ITU_R_709_2),
            )
        };
        let video = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
        let full = kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;
        for (pixel_format, matrix, expected) in [
            (video, Some(bt709), VIDEO_709),
            (full, Some(bt709), FULL_709),
            (video, Some(bt601), VIDEO_601),
            (full, Some(bt601), FULL_601),
            (video, None, VIDEO_601),
            (full, None, FULL_601),
        ] {
            assert_eq!(
                SurfaceColor::from_attachments(pixel_format, matrix).unwrap(),
                expected,
                "{pixel_format:#x} {matrix:?}"
            );
        }
    }

    #[test]
    fn other_pixel_formats_and_matrices_are_rejected() {
        let full = kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;
        for pixel_format in [kCVPixelFormatType_422YpCbCr8, kCVPixelFormatType_32BGRA] {
            let error = SurfaceColor::from_attachments(pixel_format, None).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("unsupported pixel format {pixel_format:#x}")
            );
        }
        let not_a_string = CFNumber::new_i32(1);
        for matrix in unsafe {
            [
                cf(kCVImageBufferYCbCrMatrix_ITU_R_2020),
                cf(kCVImageBufferYCbCrMatrix_SMPTE_240M_1995),
            ]
        }
        .into_iter()
        .chain([not_a_string.as_ref()])
        {
            let error = SurfaceColor::from_attachments(full, Some(matrix)).unwrap_err();
            assert!(
                error.to_string().starts_with("unsupported YCbCr matrix "),
                "{error}"
            );
        }
    }
}
