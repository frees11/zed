//! Video frames decoded outside GPUI, painted with [`Window::paint_external_surface`] and
//! drawn by the platform renderer.
//!
//! [`Window::paint_external_surface`]: crate::Window::paint_external_surface

use crate::{DevicePixels, PaintExternalSurface, Size, size};
use std::fmt;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The value of a zero chroma sample in an 8-bit plane, normalized to `0.0..=1.0`.
const CHROMA_ZERO: f32 = 128.0 / 255.0;
/// The lowest luma code of a limited-range 8-bit plane, normalized to `0.0..=1.0`.
const LIMITED_LUMA_FLOOR: f32 = 16.0 / 255.0;
/// How many 8-bit codes a limited-range luma plane spans (16 to 235).
const LIMITED_LUMA_CODES: f32 = 219.0;
/// How many 8-bit codes a limited-range chroma plane spans (16 to 240).
const LIMITED_CHROMA_CODES: f32 = 224.0;
/// How many 8-bit codes a full-range plane spans.
const FULL_RANGE_CODES: f32 = 255.0;
/// The red weight `Kr` of BT.601 luma.
const BT601_RED_LUMA: f32 = 0.299;
/// The blue weight `Kb` of BT.601 luma.
const BT601_BLUE_LUMA: f32 = 0.114;
/// The red weight `Kr` of BT.709 luma.
const BT709_RED_LUMA: f32 = 0.2126;
/// The blue weight `Kb` of BT.709 luma.
const BT709_BLUE_LUMA: f32 = 0.0722;
/// Luma samples per chroma sample along each axis.
const CHROMA_SUBSAMPLING: u32 = 2;
/// Bytes of one interleaved Cb/Cr sample pair in the chroma plane.
const CHROMA_PAIR_BYTES: u32 = 2;

/// Identifies one video stream, so a renderer can keep a stream's textures between frames
/// and upload only frames with a new generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExternalStreamId(u64);

impl ExternalStreamId {
    /// Returns an id that no other stream in this process has.
    pub fn new() -> Self {
        static NEXT_STREAM_ID: AtomicU64 = AtomicU64::new(0);
        Self(NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// How the Y'CbCr samples of a frame map to RGB: the coefficients and the code range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum YuvColorMatrix {
    /// BT.601 coefficients over the full 0–255 range of luma and chroma.
    Bt601Full,
    /// BT.709 coefficients over the limited range: luma 16–235, chroma 16–240.
    Bt709Limited,
    /// BT.709 coefficients over the full 0–255 range of luma and chroma.
    Bt709Full,
}

impl YuvColorMatrix {
    /// The 4×4 matrix that turns normalized `(Y, Cb, Cr, 1)` samples, each 8-bit code divided
    /// by 255, into `(R, G, B, 1)`. Columns come first: `matrix[1]` holds the Cb coefficients
    /// of R, G, B and alpha, so the value uploads as-is into a WGSL `mat4x4<f32>` or an HLSL
    /// `column_major float4x4`.
    pub fn ycbcr_to_rgb(self) -> [[f32; 4]; 4] {
        let (red_luma, blue_luma) = match self {
            Self::Bt601Full => (BT601_RED_LUMA, BT601_BLUE_LUMA),
            Self::Bt709Limited | Self::Bt709Full => (BT709_RED_LUMA, BT709_BLUE_LUMA),
        };
        let (luma_scale, luma_floor, chroma_scale) = match self {
            Self::Bt601Full | Self::Bt709Full => (1.0, 0.0, 1.0),
            Self::Bt709Limited => (
                FULL_RANGE_CODES / LIMITED_LUMA_CODES,
                LIMITED_LUMA_FLOOR,
                FULL_RANGE_CODES / LIMITED_CHROMA_CODES,
            ),
        };
        let green_luma = 1.0 - red_luma - blue_luma;
        let red_from_cr = 2.0 * (1.0 - red_luma) * chroma_scale;
        let blue_from_cb = 2.0 * (1.0 - blue_luma) * chroma_scale;
        let green_from_cb = -2.0 * blue_luma * (1.0 - blue_luma) / green_luma * chroma_scale;
        let green_from_cr = -2.0 * red_luma * (1.0 - red_luma) / green_luma * chroma_scale;
        let luma_bias = -luma_scale * luma_floor;
        [
            [luma_scale, luma_scale, luma_scale, 0.0],
            [0.0, green_from_cb, blue_from_cb, 0.0],
            [red_from_cr, green_from_cr, 0.0, 0.0],
            [
                luma_bias - red_from_cr * CHROMA_ZERO,
                luma_bias - (green_from_cb + green_from_cr) * CHROMA_ZERO,
                luma_bias - blue_from_cb * CHROMA_ZERO,
                1.0,
            ],
        ]
    }
}

/// The bytes of one image plane. Cloning shares the bytes; any owner works, so a decoder can
/// hand over a pooled buffer that returns to its pool when the last clone drops. The bytes
/// must not change while a frame holding them can still be drawn.
#[derive(Clone)]
pub struct PlaneBuffer(Arc<dyn AsRef<[u8]> + Send + Sync>);

impl PlaneBuffer {
    /// Wraps any owner of the plane's bytes.
    pub fn new(bytes: impl AsRef<[u8]> + Send + Sync + 'static) -> Self {
        Self(Arc::new(bytes))
    }

    /// The plane's bytes.
    pub fn bytes(&self) -> &[u8] {
        self.0.as_ref().as_ref()
    }
}

impl From<Vec<u8>> for PlaneBuffer {
    fn from(bytes: Vec<u8>) -> Self {
        Self::new(bytes)
    }
}

impl From<Arc<[u8]>> for PlaneBuffer {
    fn from(bytes: Arc<[u8]>) -> Self {
        Self::new(bytes)
    }
}

impl fmt::Debug for PlaneBuffer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlaneBuffer")
            .field("len", &self.bytes().len())
            .finish()
    }
}

/// The two planes of an NV12 frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nv12Plane {
    /// The full-resolution luma plane.
    Luma,
    /// The half-resolution plane of interleaved Cb/Cr pairs.
    Chroma,
}

impl fmt::Display for Nv12Plane {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Luma => "luma",
            Self::Chroma => "chroma",
        })
    }
}

/// Why an NV12 frame was rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Nv12FrameError {
    /// A row stride is shorter than the bytes one row of the plane holds.
    #[error("{plane} stride {stride} is shorter than its {row_bytes}-byte rows")]
    StrideTooShort {
        /// The plane with the short stride.
        plane: Nv12Plane,
        /// The stride given for the plane.
        stride: u32,
        /// The bytes one row of the plane holds.
        row_bytes: u64,
    },
    /// The chroma stride splits a Cb/Cr pair across rows, which texture uploads that address
    /// rows in whole texels cannot express.
    #[error("chroma stride {stride} splits a Cb/Cr pair across rows")]
    ChromaStrideSplitsPairs {
        /// The stride given for the chroma plane.
        stride: u32,
    },
    /// A plane buffer ends before its last row does.
    #[error("{plane} plane holds {len} bytes, its rows need {required}")]
    PlaneTooShort {
        /// The plane with the short buffer.
        plane: Nv12Plane,
        /// The length of the buffer given for the plane.
        len: usize,
        /// The bytes its rows reach up to.
        required: u64,
    },
}

/// The dimensions and row strides of an NV12 frame. The chroma plane has one Cb/Cr pair
/// for every 2×2 block of luma samples, rounded up for odd dimensions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nv12Layout {
    /// Luma samples per row.
    pub width: NonZeroU32,
    /// Luma rows.
    pub height: NonZeroU32,
    /// Bytes from the start of one luma row to the next.
    pub y_stride: u32,
    /// Bytes from the start of one chroma row to the next.
    pub uv_stride: u32,
}

impl Nv12Layout {
    /// A layout whose rows have no padding.
    pub fn packed(width: NonZeroU32, height: NonZeroU32) -> Self {
        Self {
            width,
            height,
            y_stride: width.get(),
            uv_stride: width
                .get()
                .div_ceil(CHROMA_SUBSAMPLING)
                .saturating_mul(CHROMA_PAIR_BYTES),
        }
    }

    /// Cb/Cr pairs per chroma row.
    pub fn chroma_width(&self) -> u32 {
        self.width.get().div_ceil(CHROMA_SUBSAMPLING)
    }

    /// Chroma rows.
    pub fn chroma_height(&self) -> u32 {
        self.height.get().div_ceil(CHROMA_SUBSAMPLING)
    }

    fn chroma_row_bytes(&self) -> u64 {
        u64::from(self.chroma_width()) * u64::from(CHROMA_PAIR_BYTES)
    }

    fn check_plane(
        plane: Nv12Plane,
        buffer: &PlaneBuffer,
        stride: u32,
        row_bytes: u64,
        rows: u32,
    ) -> Result<(), Nv12FrameError> {
        if u64::from(stride) < row_bytes {
            return Err(Nv12FrameError::StrideTooShort {
                plane,
                stride,
                row_bytes,
            });
        }
        let required = u64::from(stride) * u64::from(rows - 1) + row_bytes;
        let len = buffer.bytes().len();
        if (len as u64) < required {
            return Err(Nv12FrameError::PlaneTooShort {
                plane,
                len,
                required,
            });
        }
        Ok(())
    }
}

/// One NV12 frame in CPU memory: a luma plane followed by a plane of interleaved Cb/Cr
/// pairs. Cloning shares the planes. Two frames are equal when they are the same
/// generation of the same stream, which is also when a renderer reuses its upload.
#[derive(Clone)]
pub struct CpuNv12Frame {
    stream: ExternalStreamId,
    generation: u64,
    layout: Nv12Layout,
    y_plane: PlaneBuffer,
    uv_plane: PlaneBuffer,
    color: YuvColorMatrix,
}

impl CpuNv12Frame {
    /// Checks that both planes hold every row of `layout`. `generation` must change whenever
    /// the stream's pixels change: renderers upload a stream's planes only for a generation
    /// they have not drawn yet.
    pub fn new(
        stream: ExternalStreamId,
        generation: u64,
        layout: Nv12Layout,
        y_plane: impl Into<PlaneBuffer>,
        uv_plane: impl Into<PlaneBuffer>,
        color: YuvColorMatrix,
    ) -> Result<Self, Nv12FrameError> {
        let y_plane = y_plane.into();
        let uv_plane = uv_plane.into();
        Nv12Layout::check_plane(
            Nv12Plane::Luma,
            &y_plane,
            layout.y_stride,
            u64::from(layout.width.get()),
            layout.height.get(),
        )?;
        Nv12Layout::check_plane(
            Nv12Plane::Chroma,
            &uv_plane,
            layout.uv_stride,
            layout.chroma_row_bytes(),
            layout.chroma_height(),
        )?;
        if !layout.uv_stride.is_multiple_of(CHROMA_PAIR_BYTES) {
            return Err(Nv12FrameError::ChromaStrideSplitsPairs {
                stride: layout.uv_stride,
            });
        }
        Ok(Self {
            stream,
            generation,
            layout,
            y_plane,
            uv_plane,
            color,
        })
    }

    /// The stream this frame belongs to.
    pub fn stream(&self) -> ExternalStreamId {
        self.stream
    }

    /// The frame's generation within its stream.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The frame's dimensions and strides.
    pub fn layout(&self) -> Nv12Layout {
        self.layout
    }

    /// The luma plane, `layout().y_stride` bytes per row.
    pub fn y_plane(&self) -> &[u8] {
        self.y_plane.bytes()
    }

    /// The chroma plane, `layout().uv_stride` bytes per row.
    pub fn uv_plane(&self) -> &[u8] {
        self.uv_plane.bytes()
    }

    /// How the frame's samples map to RGB.
    pub fn color(&self) -> YuvColorMatrix {
        self.color
    }
}

impl PartialEq for CpuNv12Frame {
    fn eq(&self, other: &Self) -> bool {
        self.stream == other.stream && self.generation == other.generation
    }
}

impl Eq for CpuNv12Frame {}

impl fmt::Debug for CpuNv12Frame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CpuNv12Frame")
            .field("stream", &self.stream)
            .field("generation", &self.generation)
            .field("layout", &self.layout)
            .field("y_plane", &self.y_plane)
            .field("uv_plane", &self.uv_plane)
            .field("color", &self.color)
            .finish()
    }
}

/// The keys a renderer passes to `IDXGIKeyedMutex::AcquireSync` and `ReleaseSync` around
/// reading a shared texture.
///
/// A renderer takes the mutex once per generation of a stream: it copies the frame, hands the
/// texture back with `release` and draws later paints of that generation, in any window, from
/// its copy. The producer releases each frame with `acquire` once and does not need to release
/// it again for repaints.
///
/// A renderer keeps the copies of generations that are still painted, and only a limited number
/// of copies that no paint used for a while (the DirectX renderer keeps the 8 most recently
/// painted of those unused for 120 rendered frames). Beyond that limit the copy is released,
/// and a repaint of its generation draws nothing, without waiting on the mutex, until the
/// stream paints a new generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyedMutexKeys {
    /// The key the producer released the frame with.
    pub acquire: u64,
    /// The key the renderer hands the texture back with.
    pub release: u64,
}

/// One NV12 frame in a Direct3D 11 texture that another device shares through an NT handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct D3D11SharedFrame {
    /// The stream this frame belongs to.
    pub stream: ExternalStreamId,
    /// The frame's generation within its stream.
    pub generation: u64,
    /// The NT handle value from `IDXGIResource1::CreateSharedHandle`. The producer keeps the
    /// handle open while frames that name it can still be drawn.
    pub nt_handle: usize,
    /// Luma samples per row.
    pub width: NonZeroU32,
    /// Luma rows.
    pub height: NonZeroU32,
    /// The array slice of the texture that holds this frame.
    pub array_slice: u32,
    /// The keyed mutex keys, exactly when the texture was created with a keyed mutex: a frame
    /// of a keyed-mutex texture without keys, or with keys for a texture without one, is not drawn.
    pub keyed_mutex: Option<KeyedMutexKeys>,
    /// How the frame's samples map to RGB.
    pub color: YuvColorMatrix,
}

/// The kinds of [`ExternalSurface`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExternalSurfaceKind {
    /// [`ExternalSurface::CpuNv12`].
    CpuNv12,
    /// [`ExternalSurface::D3D11Shared`].
    D3D11Shared,
}

/// A video frame decoded outside GPUI. Renderers that cannot draw a kind skip it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExternalSurface {
    /// NV12 planes in CPU memory.
    CpuNv12(CpuNv12Frame),
    /// An NV12 texture shared from another Direct3D 11 device.
    D3D11Shared(D3D11SharedFrame),
}

impl ExternalSurface {
    /// The kind of this surface.
    pub fn kind(&self) -> ExternalSurfaceKind {
        match self {
            Self::CpuNv12(_) => ExternalSurfaceKind::CpuNv12,
            Self::D3D11Shared(_) => ExternalSurfaceKind::D3D11Shared,
        }
    }

    /// The stream the frame belongs to.
    pub fn stream(&self) -> ExternalStreamId {
        match self {
            Self::CpuNv12(frame) => frame.stream,
            Self::D3D11Shared(frame) => frame.stream,
        }
    }

    /// The frame's generation within its stream.
    pub fn generation(&self) -> u64 {
        match self {
            Self::CpuNv12(frame) => frame.generation,
            Self::D3D11Shared(frame) => frame.generation,
        }
    }

    /// The frame's luma dimensions.
    pub fn size(&self) -> Size<DevicePixels> {
        let (width, height) = match self {
            Self::CpuNv12(frame) => (frame.layout.width, frame.layout.height),
            Self::D3D11Shared(frame) => (frame.width, frame.height),
        };
        size(width.get().into(), height.get().into())
    }

    /// How the frame's samples map to RGB.
    pub fn color(&self) -> YuvColorMatrix {
        match self {
            Self::CpuNv12(frame) => frame.color,
            Self::D3D11Shared(frame) => frame.color,
        }
    }
}

impl From<CpuNv12Frame> for ExternalSurface {
    fn from(frame: CpuNv12Frame) -> Self {
        Self::CpuNv12(frame)
    }
}

impl From<D3D11SharedFrame> for ExternalSurface {
    fn from(frame: D3D11SharedFrame) -> Self {
        Self::D3D11Shared(frame)
    }
}

/// Remembers which kinds of external surface a renderer has reported as unsupported, so
/// it logs each kind once instead of on every frame.
#[derive(Debug, Default)]
pub struct UnsupportedExternalSurfaces {
    reported: Vec<ExternalSurfaceKind>,
}

impl UnsupportedExternalSurfaces {
    /// Records that `renderer` skips surfaces of `kind`. Logs the first skip of each kind
    /// and returns whether this call logged.
    pub fn skip(&mut self, renderer: &str, kind: ExternalSurfaceKind) -> bool {
        if self.reported.contains(&kind) {
            return false;
        }
        self.reported.push(kind);
        log::warn!("{renderer} renderer cannot draw {kind:?} external surfaces; skipping them");
        true
    }

    /// Skips every surface of a batch that `renderer` cannot draw at all.
    pub fn skip_all(&mut self, renderer: &str, surfaces: &[PaintExternalSurface]) {
        for surface in surfaces {
            self.skip(renderer, surface.surface.kind());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOLERANCE: f32 = 2.0 / 255.0;

    fn rgb(matrix: YuvColorMatrix, y: u8, cb: u8, cr: u8) -> [f32; 4] {
        let columns = matrix.ycbcr_to_rgb();
        let sample = [
            f32::from(y) / 255.0,
            f32::from(cb) / 255.0,
            f32::from(cr) / 255.0,
            1.0,
        ];
        let mut output = [0.0; 4];
        for (column, weight) in columns.iter().zip(sample) {
            for (channel, coefficient) in output.iter_mut().zip(column) {
                *channel += coefficient * weight;
            }
        }
        output
    }

    fn assert_rgb(actual: [f32; 4], expected: [f32; 3]) {
        for (channel, expected) in actual.iter().zip(expected) {
            assert!(
                (channel - expected).abs() <= TOLERANCE,
                "{actual:?} is not within {TOLERANCE} of {expected:?}"
            );
        }
        assert_eq!(actual[3], 1.0, "alpha of {actual:?}");
    }

    fn dimension(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).expect("test dimensions are not zero")
    }

    #[test]
    fn full_range_grey_keeps_its_luma() {
        let grey = 128.0 / 255.0;
        assert_rgb(rgb(YuvColorMatrix::Bt601Full, 128, 128, 128), [grey; 3]);
        assert_rgb(rgb(YuvColorMatrix::Bt709Full, 128, 128, 128), [grey; 3]);
    }

    #[test]
    fn limited_range_stretches_luma_16_to_235() {
        assert_rgb(rgb(YuvColorMatrix::Bt709Limited, 16, 128, 128), [0.0; 3]);
        assert_rgb(rgb(YuvColorMatrix::Bt709Limited, 235, 128, 128), [1.0; 3]);
        assert_rgb(
            rgb(YuvColorMatrix::Bt709Limited, 126, 128, 128),
            [128.0 / 255.0; 3],
        );
    }

    #[test]
    fn each_matrix_decodes_its_own_red() {
        assert_rgb(rgb(YuvColorMatrix::Bt601Full, 76, 85, 255), [1.0, 0.0, 0.0]);
        assert_rgb(rgb(YuvColorMatrix::Bt709Full, 54, 99, 255), [1.0, 0.0, 0.0]);
        assert_rgb(
            rgb(YuvColorMatrix::Bt709Limited, 63, 102, 240),
            [1.0, 0.0, 0.0],
        );
    }

    #[test]
    fn bt601_and_bt709_disagree_on_chroma() {
        let bt601 = rgb(YuvColorMatrix::Bt601Full, 76, 85, 255);
        let bt709 = rgb(YuvColorMatrix::Bt709Full, 76, 85, 255);
        assert!(
            (bt601[1] - bt709[1]).abs() > 10.0 * TOLERANCE,
            "{bt601:?} vs {bt709:?}"
        );
    }

    #[test]
    fn packed_layout_rounds_odd_chroma_up() {
        let layout = Nv12Layout::packed(dimension(5), dimension(3));
        assert_eq!(layout.y_stride, 5);
        assert_eq!(layout.uv_stride, 6);
        assert_eq!((layout.chroma_width(), layout.chroma_height()), (3, 2));
    }

    #[test]
    fn frame_accepts_planes_without_trailing_row_padding() {
        let layout = Nv12Layout {
            y_stride: 8,
            uv_stride: 8,
            ..Nv12Layout::packed(dimension(5), dimension(3))
        };
        let frame = CpuNv12Frame::new(
            ExternalStreamId::new(),
            7,
            layout,
            vec![0; 8 * 2 + 5],
            vec![0; 8 + 6],
            YuvColorMatrix::Bt709Limited,
        )
        .expect("planes hold every row");
        assert_eq!(frame.generation(), 7);
        assert_eq!(frame.y_plane().len(), 21);
    }

    #[test]
    fn frame_rejects_short_planes_and_strides() {
        let layout = Nv12Layout::packed(dimension(4), dimension(4));
        let stream = ExternalStreamId::new();
        let frame = |layout, y: usize, uv: usize| {
            CpuNv12Frame::new(
                stream,
                0,
                layout,
                vec![0; y],
                vec![0; uv],
                YuvColorMatrix::Bt601Full,
            )
        };
        assert_eq!(
            frame(layout, 15, 8).err(),
            Some(Nv12FrameError::PlaneTooShort {
                plane: Nv12Plane::Luma,
                len: 15,
                required: 16,
            })
        );
        assert_eq!(
            frame(layout, 16, 7).err(),
            Some(Nv12FrameError::PlaneTooShort {
                plane: Nv12Plane::Chroma,
                len: 7,
                required: 8,
            })
        );
        assert_eq!(
            frame(
                Nv12Layout {
                    y_stride: 3,
                    ..layout
                },
                64,
                8
            )
            .err(),
            Some(Nv12FrameError::StrideTooShort {
                plane: Nv12Plane::Luma,
                stride: 3,
                row_bytes: 4,
            })
        );
        assert_eq!(
            frame(
                Nv12Layout {
                    uv_stride: 3,
                    ..layout
                },
                16,
                64
            )
            .err(),
            Some(Nv12FrameError::StrideTooShort {
                plane: Nv12Plane::Chroma,
                stride: 3,
                row_bytes: 4,
            })
        );
        assert_eq!(
            frame(
                Nv12Layout {
                    uv_stride: 5,
                    ..layout
                },
                16,
                64
            )
            .err(),
            Some(Nv12FrameError::ChromaStrideSplitsPairs { stride: 5 })
        );
    }

    #[test]
    fn frames_compare_by_stream_and_generation() {
        let layout = Nv12Layout::packed(dimension(2), dimension(2));
        let stream = ExternalStreamId::new();
        let frame = |stream, generation, luma: u8| {
            CpuNv12Frame::new(
                stream,
                generation,
                layout,
                vec![luma; 4],
                vec![128; 2],
                YuvColorMatrix::Bt601Full,
            )
            .expect("packed planes")
        };
        assert_eq!(frame(stream, 1, 0), frame(stream, 1, 255));
        assert_ne!(frame(stream, 1, 0), frame(stream, 2, 0));
        assert_ne!(frame(stream, 1, 0), frame(ExternalStreamId::new(), 1, 0));
    }

    #[test]
    fn surfaces_can_move_from_a_decoder_thread() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ExternalSurface>();
    }

    #[test]
    fn stream_ids_are_unique() {
        assert_ne!(ExternalStreamId::new(), ExternalStreamId::new());
    }

    #[test]
    fn unsupported_kinds_are_reported_once_each() {
        let mut unsupported = UnsupportedExternalSurfaces::default();
        assert!(unsupported.skip("test", ExternalSurfaceKind::D3D11Shared));
        assert!(!unsupported.skip("test", ExternalSurfaceKind::D3D11Shared));
        assert!(unsupported.skip("test", ExternalSurfaceKind::CpuNv12));
        assert!(!unsupported.skip("test", ExternalSurfaceKind::CpuNv12));
    }
}
