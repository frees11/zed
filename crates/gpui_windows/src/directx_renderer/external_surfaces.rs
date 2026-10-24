//! Draws [`ExternalSurface::D3D11Shared`] frames: NV12 textures that a decoder on another
//! Direct3D 11 device shares through an NT handle.
//!
//! Each shared texture is opened once with `OpenSharedResource1` and cached by its NT handle.
//! For every new generation of a stream the renderer takes the frame's keyed mutex for at most
//! [`KEYED_MUTEX_TIMEOUT_MS`], copies the frame into an NV12 texture of its own and hands the
//! shared texture straight back. Redraws of a generation sample that copy, so they never wait
//! on the producer, and a frame that cannot be copied leaves the stream's previous frame on
//! screen, logged once until the stream copies again. Shared textures and stream copies that no
//! frame used for [`EXTERNAL_TEXTURE_IDLE_FRAMES`] rendered frames are released. A lost device
//! drops this whole component, so both caches go with the old device.

use std::{collections::hash_map::Entry, ffi::c_void, hash::Hash, num::NonZeroU32, slice};

use anyhow::{Context as _, Result};
use collections::FxHashMap;
use gpui::{
    Bounds, D3D11SharedFrame, ExternalStreamId, ExternalSurface, PaintExternalSurface,
    ScaledPixels, UnsupportedExternalSurfaces, YuvColorMatrix,
};
use windows::{
    Win32::{
        Foundation::{
            CompareObjectHandles, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, WAIT_ABANDONED,
            WAIT_TIMEOUT,
        },
        Graphics::{
            Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, D3D11_SRV_DIMENSION_TEXTURE2D},
            Direct3D11::*,
            Dxgi::{Common::*, IDXGIKeyedMutex},
        },
        System::Threading::GetCurrentProcess,
    },
    core::{HRESULT, Interface, Owned},
};

use super::{
    create_blend_state, create_constant_buffer, create_fragment_shader, create_vertex_shader,
    shader_resources::{RawShaderBytes, ShaderModule, ShaderTarget},
    update_buffer,
};

/// Rendered frames after which a stream copy or an opened shared texture that no frame used is
/// released: the count the wgpu renderer keeps CPU NV12 streams for.
pub(crate) const EXTERNAL_TEXTURE_IDLE_FRAMES: u64 = 120;

/// How long a draw waits for the producer to release a frame's keyed mutex.
pub(crate) const KEYED_MUTEX_TIMEOUT_MS: u32 = 4;

/// The name this renderer logs the external surfaces it skips under.
const RENDERER_NAME: &str = "DirectX";

/// Luma samples per chroma sample along each axis; NV12 textures have even dimensions.
const NV12_SUBSAMPLING: u32 = 2;

/// The constant buffer slot of [`Nv12SurfaceParams`], `register(b2)` in `shaders.hlsl`.
const PARAMS_SLOT: u32 = 2;

/// The shader resource slot of the luma view, `register(t2)`; the chroma view follows in `t3`.
const PLANES_SLOT: u32 = 2;

/// The sampler slot of the clamping NV12 sampler, `register(s1)`.
const SAMPLER_SLOT: u32 = 1;

/// Vertices of the triangle strip that covers one surface.
const QUAD_VERTICES: u32 = 4;

/// Values a renderer keeps between frames and releases once no frame used them for
/// [`EXTERNAL_TEXTURE_IDLE_FRAMES`] rendered frames.
struct IdleCache<K, V> {
    entries: FxHashMap<K, IdleEntry<V>>,
}

struct IdleEntry<V> {
    value: V,
    last_used_frame: u64,
}

impl<K, V> Default for IdleCache<K, V> {
    fn default() -> Self {
        Self {
            entries: FxHashMap::default(),
        }
    }
}

impl<K: Copy + Eq + Hash, V> IdleCache<K, V> {
    /// The value under `key`, marked as used in `frame`.
    fn get(&mut self, key: K, frame: u64) -> Option<&mut V> {
        let entry = self.entries.get_mut(&key)?;
        entry.last_used_frame = frame;
        Some(&mut entry.value)
    }

    /// The value under `key`, marked as used in `frame`. A value that is missing, or that
    /// `reusable` rejects, is replaced by `create`; when `create` fails, `key` holds nothing.
    fn get_or_create(
        &mut self,
        key: K,
        frame: u64,
        reusable: impl FnOnce(&V) -> bool,
        create: impl FnOnce() -> Result<V>,
    ) -> Result<&mut V> {
        if self
            .entries
            .get(&key)
            .is_some_and(|entry| !reusable(&entry.value))
        {
            self.entries.remove(&key);
        }
        let entry = match self.entries.entry(key) {
            Entry::Occupied(occupied) => occupied.into_mut(),
            Entry::Vacant(vacant) => vacant.insert(IdleEntry {
                value: create()?,
                last_used_frame: frame,
            }),
        };
        entry.last_used_frame = frame;
        Ok(&mut entry.value)
    }

    /// Stores `value` under `key`, used in `frame`, and returns the value it replaced.
    fn insert(&mut self, key: K, value: V, frame: u64) -> Option<V> {
        self.entries
            .insert(
                key,
                IdleEntry {
                    value,
                    last_used_frame: frame,
                },
            )
            .map(|entry| entry.value)
    }

    fn remove(&mut self, key: K) -> Option<V> {
        self.entries.remove(&key).map(|entry| entry.value)
    }

    /// Releases every value that no frame used for [`EXTERNAL_TEXTURE_IDLE_FRAMES`] frames up
    /// to and including `frame`.
    fn release_idle(&mut self, frame: u64) {
        self.entries
            .retain(|_, entry| frame - entry.last_used_frame < EXTERNAL_TEXTURE_IDLE_FRAMES);
    }

    #[cfg(test)]
    fn contains(&self, key: K) -> bool {
        self.entries.contains_key(&key)
    }
}

/// The size of the even NV12 texture that holds a frame of `width`×`height` luma samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Nv12Extent {
    width: u32,
    height: u32,
}

impl Nv12Extent {
    fn new(width: NonZeroU32, height: NonZeroU32) -> Result<Self> {
        let limit = D3D11_REQ_TEXTURE2D_U_OR_V_DIMENSION;
        anyhow::ensure!(
            width.get() <= limit && height.get() <= limit,
            "a {width}x{height} frame is above the {limit} texture limit of Direct3D 11"
        );
        Ok(Self {
            width: width.get().next_multiple_of(NV12_SUBSAMPLING),
            height: height.get().next_multiple_of(NV12_SUBSAMPLING),
        })
    }

    /// The texture coordinates of the frame's far corner, which an odd frame leaves short of
    /// the even texture's edge.
    fn uv_scale(self, width: NonZeroU32, height: NonZeroU32) -> [f32; 2] {
        [
            width.get() as f32 / self.width as f32,
            height.get() as f32 / self.height as f32,
        ]
    }
}

/// What `IDXGIKeyedMutex::AcquireSync` reported. Its `windows` wrapper turns the
/// `WAIT_TIMEOUT` and `WAIT_ABANDONED` success codes into `Ok`, so the raw result is mapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyedMutexWait {
    Acquired,
    TimedOut,
    /// The producer released its device while holding the mutex: the texture's contents and
    /// the mutex are no longer consistent.
    Abandoned,
    Failed(HRESULT),
}

impl From<HRESULT> for KeyedMutexWait {
    fn from(result: HRESULT) -> Self {
        match result.0 as u32 {
            0 => Self::Acquired,
            code if code == WAIT_TIMEOUT.0 => Self::TimedOut,
            code if code == WAIT_ABANDONED.0 => Self::Abandoned,
            _ => Self::Failed(result),
        }
    }
}

fn acquire_keyed_mutex(mutex: &IDXGIKeyedMutex, key: u64, timeout_ms: u32) -> KeyedMutexWait {
    KeyedMutexWait::from(unsafe {
        (Interface::vtable(mutex).AcquireSync)(Interface::as_raw(mutex), key, timeout_ms)
    })
}

/// The constant buffer of one NV12 draw, `cbuffer Nv12SurfaceParams` in `shaders.hlsl`.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
struct Nv12SurfaceParams {
    bounds: Bounds<ScaledPixels>,
    content_mask: Bounds<ScaledPixels>,
    uv_scale: [f32; 2],
    padding: [f32; 2],
    ycbcr_to_rgb: [[f32; 4]; 4],
}

const _: () = assert!(std::mem::size_of::<Nv12SurfaceParams>() == 112);

impl Nv12SurfaceParams {
    /// The parameters that draw `copy` over `painted`, with the matrix the wgpu renderer
    /// uploads for the same colour space.
    fn new(painted: &PaintExternalSurface, copy: &CopiedFrame) -> Self {
        Self {
            bounds: painted.bounds,
            content_mask: painted.content_mask.bounds,
            uv_scale: copy.uv_scale,
            padding: [0.0; 2],
            ycbcr_to_rgb: copy.color.ycbcr_to_rgb(),
        }
    }
}

/// A texture shared through an NT handle, opened on the renderer's device.
struct SharedTexture {
    /// The renderer's own handle to the texture, so that a later frame whose handle value the
    /// producer has since reused for another object is recognised as a different texture.
    handle: Owned<HANDLE>,
    texture: ID3D11Texture2D,
    keyed_mutex: Option<IDXGIKeyedMutex>,
    desc: D3D11_TEXTURE2D_DESC,
}

impl SharedTexture {
    fn open(device: &ID3D11Device1, nt_handle: usize) -> Result<Self> {
        let shared = HANDLE(nt_handle as *mut c_void);
        let mut duplicate = HANDLE::default();
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                shared,
                GetCurrentProcess(),
                &mut duplicate,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )
        }
        .with_context(|| format!("duplicating shared texture handle {nt_handle:#x}"))?;
        let handle = unsafe { Owned::new(duplicate) };
        let texture: ID3D11Texture2D = unsafe { device.OpenSharedResource1(shared) }
            .with_context(|| format!("opening shared texture handle {nt_handle:#x}"))?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        anyhow::ensure!(
            desc.Format == DXGI_FORMAT_NV12,
            "shared texture handle {nt_handle:#x} holds {:?}, not NV12",
            desc.Format
        );
        Ok(Self {
            handle,
            keyed_mutex: texture.cast().ok(),
            texture,
            desc,
        })
    }

    fn is_named_by(&self, nt_handle: usize) -> bool {
        unsafe { CompareObjectHandles(*self.handle, HANDLE(nt_handle as *mut c_void)) }.as_bool()
    }
}

/// The frame a stream copy last received.
#[derive(Clone, Copy, Debug)]
struct CopiedFrame {
    generation: u64,
    uv_scale: [f32; 2],
    color: YuvColorMatrix,
}

/// The renderer's own NV12 texture for one stream and its luma and chroma views.
struct StreamCopy {
    extent: Nv12Extent,
    texture: ID3D11Texture2D,
    planes: [Option<ID3D11ShaderResourceView>; 2],
    frame: Option<CopiedFrame>,
}

impl StreamCopy {
    fn new(device: &ID3D11Device, extent: Nv12Extent) -> Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: extent.width,
            Height: extent.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.with_context(|| {
            format!("creating a {}x{} NV12 texture", extent.width, extent.height)
        })?;
        let texture = texture.context("CreateTexture2D returned no NV12 texture")?;
        let plane_view = |format| -> Result<Option<ID3D11ShaderResourceView>> {
            let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_SRV {
                        MostDetailedMip: 0,
                        MipLevels: 1,
                    },
                },
            };
            let mut view = None;
            unsafe { device.CreateShaderResourceView(&texture, Some(&desc), Some(&mut view)) }
                .with_context(|| format!("creating the {format:?} view of an NV12 texture"))?;
            Ok(view)
        };
        let planes = [
            plane_view(DXGI_FORMAT_R8_UNORM)?,
            plane_view(DXGI_FORMAT_R8G8_UNORM)?,
        ];
        Ok(Self {
            extent,
            texture,
            planes,
            frame: None,
        })
    }
}

/// The subresource index of the most detailed mip of `array_slice`, as `D3D11CalcSubresource`
/// computes it in the Direct3D 11 headers, which the `windows` crate does not carry.
fn top_mip_subresource(array_slice: u32, mip_levels: u32) -> u32 {
    array_slice * mip_levels
}

/// How taking a frame out of its shared texture ended, when nothing failed outright.
enum FrameCopy {
    Copied,
    TimedOut { key: u64 },
    Abandoned,
}

/// Copies `frame` out of `shared` into `copy`, under the frame's keyed mutex when it names one.
fn copy_shared_frame(
    context: &ID3D11DeviceContext,
    shared: &SharedTexture,
    frame: &D3D11SharedFrame,
    copy: &StreamCopy,
) -> Result<FrameCopy> {
    let desc = &shared.desc;
    anyhow::ensure!(
        frame.width.get() <= desc.Width && frame.height.get() <= desc.Height,
        "a {}x{} frame does not fit its {}x{} shared texture",
        frame.width,
        frame.height,
        desc.Width,
        desc.Height
    );
    anyhow::ensure!(
        frame.array_slice < desc.ArraySize,
        "array slice {} is outside the {} slices of its shared texture",
        frame.array_slice,
        desc.ArraySize
    );
    let source_subresource = top_mip_subresource(frame.array_slice, desc.MipLevels);
    let region = D3D11_BOX {
        left: 0,
        top: 0,
        front: 0,
        right: copy.extent.width,
        bottom: copy.extent.height,
        back: 1,
    };
    let copy_region = || unsafe {
        context.CopySubresourceRegion(
            &copy.texture,
            0,
            0,
            0,
            0,
            &shared.texture,
            source_subresource,
            Some(&region),
        )
    };

    let Some(keys) = frame.keyed_mutex else {
        copy_region();
        return Ok(FrameCopy::Copied);
    };
    let mutex = shared
        .keyed_mutex
        .as_ref()
        .context("the frame names keyed mutex keys, its shared texture has no keyed mutex")?;
    match acquire_keyed_mutex(mutex, keys.acquire, KEYED_MUTEX_TIMEOUT_MS) {
        KeyedMutexWait::Acquired => {
            copy_region();
            unsafe { mutex.ReleaseSync(keys.release) }
                .with_context(|| format!("releasing keyed mutex key {}", keys.release))?;
            Ok(FrameCopy::Copied)
        }
        KeyedMutexWait::TimedOut => Ok(FrameCopy::TimedOut { key: keys.acquire }),
        KeyedMutexWait::Abandoned => Ok(FrameCopy::Abandoned),
        KeyedMutexWait::Failed(result) => Err(windows::core::Error::from_hresult(result))
            .with_context(|| format!("acquiring keyed mutex key {}", keys.acquire)),
    }
}

/// The shaders and fixed state of the NV12 draw.
struct Nv12Pipeline {
    vertex: ID3D11VertexShader,
    fragment: ID3D11PixelShader,
    params: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    blend_state: ID3D11BlendState,
}

impl Nv12Pipeline {
    fn new(device: &ID3D11Device) -> Result<Self> {
        let vertex = {
            let shader = RawShaderBytes::new(ShaderModule::Nv12Surface, ShaderTarget::Vertex)?;
            create_vertex_shader(device, shader.as_bytes())?
        };
        let fragment = {
            let shader = RawShaderBytes::new(ShaderModule::Nv12Surface, ShaderTarget::Fragment)?;
            create_fragment_shader(device, shader.as_bytes())?
        };
        let params = create_constant_buffer::<Nv12SurfaceParams>(device)?
            .context("CreateBuffer returned no NV12 constant buffer")?;
        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MipLODBias: 0.0,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D11_COMPARISON_ALWAYS,
            BorderColor: [0.0; 4],
            MinLOD: 0.0,
            MaxLOD: D3D11_FLOAT32_MAX,
        };
        let mut sampler = None;
        unsafe { device.CreateSamplerState(&sampler_desc, Some(&mut sampler)) }
            .context("creating the NV12 sampler")?;
        Ok(Self {
            vertex,
            fragment,
            params,
            sampler: sampler.context("CreateSamplerState returned no NV12 sampler")?,
            blend_state: create_blend_state(device)?,
        })
    }

    fn draw(
        &self,
        context: &ID3D11DeviceContext,
        params: &Nv12SurfaceParams,
        planes: &[Option<ID3D11ShaderResourceView>; 2],
    ) -> Result<()> {
        update_buffer(context, &self.params, slice::from_ref(params))?;
        let params = [Some(self.params.clone())];
        let samplers = [Some(self.sampler.clone())];
        unsafe {
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            context.VSSetShader(&self.vertex, None);
            context.PSSetShader(&self.fragment, None);
            context.OMSetBlendState(&self.blend_state, None, u32::MAX);
            context.VSSetConstantBuffers(PARAMS_SLOT, Some(&params));
            context.PSSetConstantBuffers(PARAMS_SLOT, Some(&params));
            context.PSSetShaderResources(PLANES_SLOT, Some(planes));
            context.PSSetSamplers(SAMPLER_SLOT, Some(&samplers));
            context.Draw(QUAD_VERTICES, 0);
        }
        Ok(())
    }
}

/// Draws the external surfaces of a scene and keeps their textures between frames.
pub(super) struct ExternalSurfaces {
    device: ID3D11Device,
    device1: ID3D11Device1,
    pipeline: Nv12Pipeline,
    shared_textures: IdleCache<usize, SharedTexture>,
    streams: IdleCache<ExternalStreamId, StreamCopy>,
    failing_streams: IdleCache<ExternalStreamId, ()>,
    unsupported: UnsupportedExternalSurfaces,
    frame: u64,
}

impl ExternalSurfaces {
    pub(super) fn new(device: &ID3D11Device) -> Result<Self> {
        Ok(Self {
            device: device.clone(),
            device1: device
                .cast()
                .context("the Direct3D 11.1 device interface opens shared textures")?,
            pipeline: Nv12Pipeline::new(device).context("creating the NV12 surface pipeline")?,
            shared_textures: IdleCache::default(),
            streams: IdleCache::default(),
            failing_streams: IdleCache::default(),
            unsupported: UnsupportedExternalSurfaces::default(),
            frame: 0,
        })
    }

    /// Draws `surfaces` in order. A surface whose frame cannot be copied shows its stream's
    /// previous frame, or nothing when no frame of the stream's current size has been copied
    /// yet, and is logged once until the stream copies again.
    pub(super) fn draw(
        &mut self,
        context: &ID3D11DeviceContext,
        surfaces: &[PaintExternalSurface],
    ) -> Result<()> {
        for painted in surfaces {
            let ExternalSurface::D3D11Shared(frame) = &painted.surface else {
                self.unsupported.skip(RENDERER_NAME, painted.surface.kind());
                continue;
            };
            let uploaded = self
                .streams
                .get(frame.stream, self.frame)
                .and_then(|copy| copy.frame)
                .is_some_and(|copied| copied.generation == frame.generation);
            if !uploaded {
                match self.upload(context, frame) {
                    Ok(()) => {
                        if self.failing_streams.remove(frame.stream).is_some() {
                            log::info!(
                                "{RENDERER_NAME} renderer copies frames of external surface stream {:?} again",
                                frame.stream
                            );
                        }
                    }
                    Err(error) => self.report_failure(frame.stream, &error),
                }
            }
            let Some(copy) = self.streams.get(frame.stream, self.frame) else {
                continue;
            };
            let Some(copied) = &copy.frame else {
                continue;
            };
            let params = Nv12SurfaceParams::new(painted, copied);
            self.pipeline.draw(context, &params, &copy.planes)?;
        }
        Ok(())
    }

    /// Ends a rendered frame: releases the textures no frame used for
    /// [`EXTERNAL_TEXTURE_IDLE_FRAMES`] frames.
    pub(super) fn finish_frame(&mut self) {
        self.shared_textures.release_idle(self.frame);
        self.streams.release_idle(self.frame);
        self.failing_streams.release_idle(self.frame);
        self.frame += 1;
    }

    fn upload(&mut self, context: &ID3D11DeviceContext, frame: &D3D11SharedFrame) -> Result<()> {
        let extent = Nv12Extent::new(frame.width, frame.height)?;
        let device = &self.device;
        let copy = self.streams.get_or_create(
            frame.stream,
            self.frame,
            |copy| copy.extent == extent,
            || StreamCopy::new(device, extent),
        )?;
        let device1 = &self.device1;
        let shared = self.shared_textures.get_or_create(
            frame.nt_handle,
            self.frame,
            |shared| shared.is_named_by(frame.nt_handle),
            || SharedTexture::open(device1, frame.nt_handle),
        )?;
        match copy_shared_frame(context, shared, frame, copy)? {
            FrameCopy::Copied => {
                copy.frame = Some(CopiedFrame {
                    generation: frame.generation,
                    uv_scale: extent.uv_scale(frame.width, frame.height),
                    color: frame.color,
                });
                Ok(())
            }
            FrameCopy::TimedOut { key } => Err(anyhow::anyhow!(
                "the producer did not release keyed mutex key {key} within {KEYED_MUTEX_TIMEOUT_MS} ms"
            )),
            FrameCopy::Abandoned => {
                self.shared_textures.remove(frame.nt_handle);
                Err(anyhow::anyhow!(
                    "the producer abandoned the keyed mutex of shared texture handle {:#x}",
                    frame.nt_handle
                ))
            }
        }
    }

    fn report_failure(&mut self, stream: ExternalStreamId, error: &anyhow::Error) {
        if self
            .failing_streams
            .insert(stream, (), self.frame)
            .is_none()
        {
            log::error!(
                "{RENDERER_NAME} renderer cannot copy a frame of external surface stream {stream:?}, keeping its last frame: {error:#}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENTRY: u32 = 7;

    fn dimension(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).expect("test dimensions are not zero")
    }

    fn cache_with_entry_used_in(frame: u64) -> IdleCache<u32, &'static str> {
        let mut cache = IdleCache::default();
        assert!(cache.insert(ENTRY, "texture", frame).is_none());
        cache
    }

    #[test]
    fn unused_entries_survive_119_frames_and_go_on_the_120th() {
        let mut cache = cache_with_entry_used_in(1);
        for frame in 1..EXTERNAL_TEXTURE_IDLE_FRAMES + 1 {
            cache.release_idle(frame);
        }
        assert!(cache.contains(ENTRY));
        cache.release_idle(EXTERNAL_TEXTURE_IDLE_FRAMES + 1);
        assert!(!cache.contains(ENTRY));
    }

    #[test]
    fn entries_used_every_frame_stay() {
        let mut cache = cache_with_entry_used_in(0);
        for frame in 0..3 * EXTERNAL_TEXTURE_IDLE_FRAMES {
            assert_eq!(cache.get(ENTRY, frame).copied(), Some("texture"));
            cache.release_idle(frame);
        }
        assert!(cache.contains(ENTRY));
    }

    #[test]
    fn get_or_create_keeps_a_reusable_entry() -> Result<()> {
        let mut cache = cache_with_entry_used_in(0);
        let value = cache.get_or_create(ENTRY, 100, |_| true, || anyhow::bail!("not called"))?;
        assert_eq!(*value, "texture");
        cache.release_idle(100 + EXTERNAL_TEXTURE_IDLE_FRAMES - 1);
        assert!(
            cache.contains(ENTRY),
            "get_or_create marks the entry as used"
        );
        Ok(())
    }

    #[test]
    fn get_or_create_replaces_an_entry_it_cannot_reuse() -> Result<()> {
        let mut cache = cache_with_entry_used_in(0);
        let value = cache.get_or_create(ENTRY, 1, |_| false, || Ok("reopened"))?;
        assert_eq!(*value, "reopened");
        Ok(())
    }

    #[test]
    fn a_failed_replacement_leaves_no_stale_entry() {
        let mut cache = cache_with_entry_used_in(0);
        let result = cache.get_or_create(ENTRY, 1, |_| false, || anyhow::bail!("open failed"));
        assert!(result.is_err());
        assert!(!cache.contains(ENTRY));
    }

    #[test]
    fn nv12_extent_rounds_odd_frames_up_to_even() -> Result<()> {
        let extent = Nv12Extent::new(dimension(1365), dimension(767))?;
        assert_eq!(
            extent,
            Nv12Extent {
                width: 1366,
                height: 768
            }
        );
        assert_eq!(
            extent.uv_scale(dimension(1365), dimension(767)),
            [1365.0 / 1366.0, 767.0 / 768.0]
        );
        let even = Nv12Extent::new(dimension(1920), dimension(1080))?;
        assert_eq!(even.uv_scale(dimension(1920), dimension(1080)), [1.0, 1.0]);
        Ok(())
    }

    #[test]
    fn nv12_extent_rejects_frames_above_the_texture_limit() {
        let limit = D3D11_REQ_TEXTURE2D_U_OR_V_DIMENSION;
        assert!(Nv12Extent::new(dimension(limit), dimension(limit)).is_ok());
        assert!(Nv12Extent::new(dimension(limit + 1), dimension(2)).is_err());
        assert!(Nv12Extent::new(dimension(2), dimension(u32::MAX)).is_err());
    }

    #[test]
    fn subresources_count_every_mip_of_the_earlier_slices() {
        assert_eq!(top_mip_subresource(0, 1), 0);
        assert_eq!(top_mip_subresource(2, 1), 2);
        assert_eq!(top_mip_subresource(2, 3), 6);
    }

    #[test]
    fn keyed_mutex_results_map_to_waits() {
        let code = |value: u32| HRESULT(value as i32);
        assert_eq!(KeyedMutexWait::from(code(0)), KeyedMutexWait::Acquired);
        assert_eq!(
            KeyedMutexWait::from(code(WAIT_TIMEOUT.0)),
            KeyedMutexWait::TimedOut
        );
        assert_eq!(
            KeyedMutexWait::from(code(WAIT_ABANDONED.0)),
            KeyedMutexWait::Abandoned
        );
        let removed = windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED;
        assert_eq!(
            KeyedMutexWait::from(removed),
            KeyedMutexWait::Failed(removed)
        );
    }

    const SHADERS: &str = include_str!("../shaders.hlsl");

    /// An 8-bit code of a normalized channel, for comparing colours level by level.
    fn level(channel: f32) -> f32 {
        channel * 255.0
    }

    fn shared_frame(width: u32, height: u32, color: YuvColorMatrix) -> D3D11SharedFrame {
        D3D11SharedFrame {
            stream: ExternalStreamId::new(),
            generation: 1,
            nt_handle: 0,
            width: dimension(width),
            height: dimension(height),
            array_slice: 0,
            keyed_mutex: None,
            color,
        }
    }

    fn painted(frame: D3D11SharedFrame, side: f32) -> PaintExternalSurface {
        let bounds = Bounds {
            origin: gpui::point(ScaledPixels(0.0), ScaledPixels(0.0)),
            size: gpui::size(ScaledPixels(side), ScaledPixels(side)),
        };
        PaintExternalSurface {
            order: 0,
            bounds,
            content_mask: gpui::ContentMask { bounds },
            surface: frame.into(),
        }
    }

    fn params_for(color: YuvColorMatrix) -> Nv12SurfaceParams {
        let frame = shared_frame(16, 16, color);
        let copied = CopiedFrame {
            generation: frame.generation,
            uv_scale: [1.0, 1.0],
            color,
        };
        Nv12SurfaceParams::new(&painted(frame, 16.0), &copied)
    }

    /// What `mul(nv12_ycbcr_to_rgb, float4(y, cb, cr, 1))` computes in `shaders.hlsl` for the
    /// 16 floats uploaded at the matrix offset of the constant buffer, read as HLSL reads a
    /// `column_major float4x4`: element (row, column) sits at `column * 4 + row`.
    fn hlsl_decode(params: &Nv12SurfaceParams, (y, cb, cr): (u8, u8, u8)) -> [f32; 4] {
        let uploaded = params.ycbcr_to_rgb.as_flattened();
        let sample = [
            f32::from(y) / 255.0,
            f32::from(cb) / 255.0,
            f32::from(cr) / 255.0,
            1.0,
        ];
        let mut output = [0.0; 4];
        for (row, channel) in output.iter_mut().enumerate() {
            *channel = (0..4)
                .map(|column| uploaded[column * 4 + row] * sample[column])
                .sum();
        }
        output
    }

    #[test]
    fn params_upload_the_wgpu_matrix_of_each_color() {
        for color in [
            YuvColorMatrix::Bt601Full,
            YuvColorMatrix::Bt709Limited,
            YuvColorMatrix::Bt709Full,
        ] {
            assert_eq!(
                params_for(color).ycbcr_to_rgb,
                color.ycbcr_to_rgb(),
                "{color:?}"
            );
        }
    }

    #[test]
    fn hlsl_reads_the_uploaded_matrix_as_each_colors_red_and_grey() {
        const TOLERANCE_LEVELS: f32 = 2.0;
        let cases = [
            (YuvColorMatrix::Bt601Full, (76, 85, 255), [255.0, 0.0, 0.0]),
            (
                YuvColorMatrix::Bt709Limited,
                (63, 102, 240),
                [255.0, 0.0, 0.0],
            ),
            (YuvColorMatrix::Bt709Full, (54, 99, 255), [255.0, 0.0, 0.0]),
            (YuvColorMatrix::Bt601Full, (128, 128, 128), [128.0; 3]),
            (YuvColorMatrix::Bt709Limited, (126, 128, 128), [128.0; 3]),
            (YuvColorMatrix::Bt709Limited, (16, 128, 128), [0.0; 3]),
            (YuvColorMatrix::Bt709Limited, (235, 128, 128), [255.0; 3]),
        ];
        for (color, samples, expected) in cases {
            let decoded = hlsl_decode(&params_for(color), samples);
            for (channel, expected) in decoded.iter().zip(expected) {
                assert!(
                    (level(*channel) - expected).abs() <= TOLERANCE_LEVELS,
                    "{color:?} {samples:?} decoded to {decoded:?}"
                );
            }
            assert_eq!(decoded[3], 1.0, "{color:?} alpha");
        }
    }

    #[test]
    fn params_match_the_hlsl_constant_buffer() {
        assert_eq!(std::mem::offset_of!(Nv12SurfaceParams, bounds), 0);
        assert_eq!(std::mem::offset_of!(Nv12SurfaceParams, content_mask), 16);
        assert_eq!(std::mem::offset_of!(Nv12SurfaceParams, uv_scale), 32);
        assert_eq!(std::mem::offset_of!(Nv12SurfaceParams, ycbcr_to_rgb), 48);
        let declarations = [
            format!("cbuffer Nv12SurfaceParams: register(b{PARAMS_SLOT})"),
            "column_major float4x4 nv12_ycbcr_to_rgb;".to_string(),
            format!("Texture2D<float> t_nv12_luma: register(t{PLANES_SLOT})"),
            format!(
                "Texture2D<float2> t_nv12_chroma: register(t{})",
                PLANES_SLOT + 1
            ),
            format!("SamplerState s_nv12: register(s{SAMPLER_SLOT})"),
            "mul(nv12_ycbcr_to_rgb, float4(luma, chroma, 1.0))".to_string(),
        ];
        for declaration in declarations {
            assert!(
                SHADERS.contains(&declaration),
                "shaders.hlsl lacks `{declaration}`"
            );
        }
    }

    #[test]
    fn params_carry_bounds_mask_and_uv_scale() {
        let frame = shared_frame(15, 9, YuvColorMatrix::Bt709Limited);
        let extent = Nv12Extent::new(frame.width, frame.height).expect("small frame");
        let surface = painted(frame, 32.0);
        let copied = CopiedFrame {
            generation: frame.generation,
            uv_scale: extent.uv_scale(frame.width, frame.height),
            color: frame.color,
        };
        let params = Nv12SurfaceParams::new(&surface, &copied);
        assert_eq!(params.bounds, surface.bounds);
        assert_eq!(params.content_mask, surface.content_mask.bounds);
        assert_eq!(params.uv_scale, [15.0 / 16.0, 9.0 / 10.0]);
    }

    /// Draws real shared textures on WARP, the software rasterizer every Windows 10+ install
    /// carries, with the decoder side on a second WARP device.
    mod warp {
        use super::*;
        use crate::directx_renderer::{
            GlobalParams, read_back_render_target, set_rasterizer_state,
        };
        use windows::{
            Win32::{
                Foundation::HMODULE,
                Graphics::{
                    Direct3D::{
                        D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
                    },
                    Dxgi::{DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIResource1},
                },
            },
            core::PCWSTR,
        };

        const SIDE: u32 = 16;
        /// The key a new keyed mutex starts released with, and the key the renderer hands a
        /// frame back with.
        const PRODUCER_KEY: u64 = 0;
        /// The key the producer releases a finished frame with.
        const FRAME_READY_KEY: u64 = 1;
        const PRODUCER_TIMEOUT_MS: u32 = 1000;
        const GREY: [u8; 4] = [128, 128, 128, 255];
        const BLACK: [u8; 4] = [0, 0, 0, 255];
        const WHITE: [u8; 4] = [255, 255, 255, 255];
        const RED: [u8; 4] = [255, 0, 0, 255];
        const BT601_FULL_TOLERANCE: u8 = 2;
        const BT709_TOLERANCE: u8 = 3;

        struct Warp {
            device: ID3D11Device,
            context: ID3D11DeviceContext,
        }

        impl Warp {
            fn new() -> Result<Self> {
                let mut device = None;
                let mut context = None;
                unsafe {
                    D3D11CreateDevice(
                        None,
                        D3D_DRIVER_TYPE_WARP,
                        HMODULE::default(),
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                        Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                        D3D11_SDK_VERSION,
                        Some(&mut device),
                        None,
                        Some(&mut context),
                    )
                }
                .context("creating a WARP device")?;
                Ok(Self {
                    device: device.context("D3D11CreateDevice returned no device")?,
                    context: context.context("D3D11CreateDevice returned no context")?,
                })
            }
        }

        /// A `SIDE`×`SIDE` B8G8R8A8 render target bound the way `DirectXRenderer::pre_draw`
        /// binds the window's.
        struct Target {
            texture: ID3D11Texture2D,
            view: Option<ID3D11RenderTargetView>,
            globals: Option<ID3D11Buffer>,
        }

        impl Target {
            fn new(warp: &Warp) -> Result<Self> {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: SIDE,
                    Height: SIDE,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut texture = None;
                unsafe { warp.device.CreateTexture2D(&desc, None, Some(&mut texture)) }?;
                let texture = texture.context("CreateTexture2D returned no render target")?;
                let mut view = None;
                unsafe {
                    warp.device
                        .CreateRenderTargetView(&texture, None, Some(&mut view))
                }?;
                let globals = create_constant_buffer::<GlobalParams>(&warp.device)?;
                update_buffer(
                    &warp.context,
                    globals.as_ref().context("no global params buffer")?,
                    &[GlobalParams {
                        viewport_size: [SIDE as f32; 2],
                        ..GlobalParams::default()
                    }],
                )?;
                Ok(Self {
                    texture,
                    view,
                    globals,
                })
            }

            fn render(
                &self,
                warp: &Warp,
                surfaces: &mut ExternalSurfaces,
                frame: D3D11SharedFrame,
            ) -> Result<image::RgbaImage> {
                let viewport = D3D11_VIEWPORT {
                    TopLeftX: 0.0,
                    TopLeftY: 0.0,
                    Width: SIDE as f32,
                    Height: SIDE as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                };
                let view = self.view.as_ref().context("no render target view")?;
                unsafe {
                    warp.context.ClearRenderTargetView(view, &[0.0; 4]);
                    warp.context
                        .OMSetRenderTargets(Some(slice::from_ref(&self.view)), None);
                    warp.context.RSSetViewports(Some(&[viewport]));
                    warp.context
                        .VSSetConstantBuffers(0, Some(slice::from_ref(&self.globals)));
                }
                set_rasterizer_state(&warp.device, &warp.context)?;
                surfaces.draw(&warp.context, &[painted(frame, SIDE as f32)])?;
                surfaces.finish_frame();
                read_back_render_target(&warp.device, &warp.context, &self.texture)
            }
        }

        /// A decoder's NV12 frame of uniform samples in a texture shared through an NT handle,
        /// guarded by a keyed mutex.
        struct SharedNv12 {
            _texture: ID3D11Texture2D,
            mutex: IDXGIKeyedMutex,
            handle: Owned<HANDLE>,
        }

        impl SharedNv12 {
            fn new(producer: &Warp, (y, cb, cr): (u8, u8, u8)) -> Result<Self> {
                let luma_bytes = (SIDE * SIDE) as usize;
                let chroma_pairs = (SIDE * SIDE / 4) as usize;
                let mut planes = vec![y; luma_bytes];
                planes.extend([cb, cr].repeat(chroma_pairs));
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: SIDE,
                    Height: SIDE,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0
                        | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0)
                        as u32,
                };
                let initial = D3D11_SUBRESOURCE_DATA {
                    pSysMem: planes.as_ptr().cast(),
                    SysMemPitch: SIDE,
                    SysMemSlicePitch: 0,
                };
                let mut texture = None;
                unsafe {
                    producer
                        .device
                        .CreateTexture2D(&desc, Some(&initial), Some(&mut texture))
                }
                .context("creating a shared NV12 texture")?;
                let texture = texture.context("CreateTexture2D returned no NV12 texture")?;
                let resource: IDXGIResource1 = texture.cast()?;
                let handle = unsafe {
                    resource.CreateSharedHandle(
                        None,
                        DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                        PCWSTR::null(),
                    )
                }
                .context("creating the NT handle")?;
                Ok(Self {
                    mutex: texture.cast()?,
                    _texture: texture,
                    handle: unsafe { Owned::new(handle) },
                })
            }

            /// Hands the frame to the renderer the way a decoder does after writing it.
            fn release_frame(&self) -> Result<()> {
                let wait = acquire_keyed_mutex(&self.mutex, PRODUCER_KEY, PRODUCER_TIMEOUT_MS);
                anyhow::ensure!(
                    wait == KeyedMutexWait::Acquired,
                    "producer could not take key {PRODUCER_KEY} back: {wait:?}"
                );
                unsafe { self.mutex.ReleaseSync(FRAME_READY_KEY) }?;
                Ok(())
            }

            fn frame(
                &self,
                stream: ExternalStreamId,
                generation: u64,
                color: YuvColorMatrix,
            ) -> D3D11SharedFrame {
                D3D11SharedFrame {
                    stream,
                    generation,
                    nt_handle: self.handle.0 as usize,
                    width: dimension(SIDE),
                    height: dimension(SIDE),
                    array_slice: 0,
                    keyed_mutex: Some(gpui::KeyedMutexKeys {
                        acquire: FRAME_READY_KEY,
                        release: PRODUCER_KEY,
                    }),
                    color,
                }
            }
        }

        fn assert_every_pixel(image: &image::RgbaImage, expected: [u8; 4], tolerance: u8) {
            for (x, y, pixel) in image.enumerate_pixels() {
                assert!(
                    pixel
                        .0
                        .iter()
                        .zip(expected)
                        .all(|(actual, expected)| actual.abs_diff(expected) <= tolerance),
                    "pixel ({x}, {y}) was {:?}, expected {expected:?} within {tolerance}",
                    pixel.0
                );
            }
        }

        fn copied_generation(
            surfaces: &mut ExternalSurfaces,
            stream: ExternalStreamId,
        ) -> Option<u64> {
            let frame = surfaces.frame;
            surfaces
                .streams
                .get(stream, frame)
                .and_then(|copy| copy.frame)
                .map(|copied| copied.generation)
        }

        #[test]
        #[ignore = "hw"]
        fn warp_draws_shared_nv12_frames_in_each_matrix() -> Result<()> {
            let renderer = Warp::new()?;
            let producer = Warp::new()?;
            let target = Target::new(&renderer)?;
            let mut surfaces = ExternalSurfaces::new(&renderer.device)?;
            let cases = [
                (
                    (128, 128, 128),
                    YuvColorMatrix::Bt601Full,
                    GREY,
                    BT601_FULL_TOLERANCE,
                ),
                (
                    (126, 128, 128),
                    YuvColorMatrix::Bt709Limited,
                    GREY,
                    BT709_TOLERANCE,
                ),
                (
                    (16, 128, 128),
                    YuvColorMatrix::Bt709Limited,
                    BLACK,
                    BT709_TOLERANCE,
                ),
                (
                    (235, 128, 128),
                    YuvColorMatrix::Bt709Limited,
                    WHITE,
                    BT709_TOLERANCE,
                ),
                (
                    (76, 85, 255),
                    YuvColorMatrix::Bt601Full,
                    RED,
                    BT709_TOLERANCE,
                ),
                (
                    (63, 102, 240),
                    YuvColorMatrix::Bt709Limited,
                    RED,
                    BT709_TOLERANCE,
                ),
                (
                    (54, 99, 255),
                    YuvColorMatrix::Bt709Full,
                    RED,
                    BT709_TOLERANCE,
                ),
            ];
            for (samples, color, expected, tolerance) in cases {
                let shared = SharedNv12::new(&producer, samples)?;
                shared.release_frame()?;
                let frame = shared.frame(ExternalStreamId::new(), 1, color);
                let image = target.render(&renderer, &mut surfaces, frame)?;
                assert_every_pixel(&image, expected, tolerance);
            }
            Ok(())
        }

        #[test]
        #[ignore = "hw"]
        fn warp_redraws_a_generation_from_its_copy_and_hands_the_texture_back() -> Result<()> {
            let renderer = Warp::new()?;
            let producer = Warp::new()?;
            let target = Target::new(&renderer)?;
            let mut surfaces = ExternalSurfaces::new(&renderer.device)?;
            let shared = SharedNv12::new(&producer, (128, 128, 128))?;
            let stream = ExternalStreamId::new();
            let frame = |generation| shared.frame(stream, generation, YuvColorMatrix::Bt601Full);

            shared.release_frame()?;
            for _ in 0..3 {
                let image = target.render(&renderer, &mut surfaces, frame(1))?;
                assert_every_pixel(&image, GREY, BT601_FULL_TOLERANCE);
            }
            assert_eq!(copied_generation(&mut surfaces, stream), Some(1));

            let image = target.render(&renderer, &mut surfaces, frame(2))?;
            assert_every_pixel(&image, GREY, BT601_FULL_TOLERANCE);
            assert_eq!(
                copied_generation(&mut surfaces, stream),
                Some(1),
                "a frame the producer has not released keeps the previous copy"
            );

            shared.release_frame()?;
            target.render(&renderer, &mut surfaces, frame(2))?;
            assert_eq!(copied_generation(&mut surfaces, stream), Some(2));
            assert_eq!(surfaces.shared_textures.entries.len(), 1);
            Ok(())
        }

        #[test]
        #[ignore = "hw"]
        fn warp_releases_idle_streams_and_shared_textures() -> Result<()> {
            let renderer = Warp::new()?;
            let producer = Warp::new()?;
            let target = Target::new(&renderer)?;
            let mut surfaces = ExternalSurfaces::new(&renderer.device)?;
            let shared = SharedNv12::new(&producer, (128, 128, 128))?;
            let stream = ExternalStreamId::new();
            shared.release_frame()?;
            target.render(
                &renderer,
                &mut surfaces,
                shared.frame(stream, 1, YuvColorMatrix::Bt601Full),
            )?;
            for _ in 0..EXTERNAL_TEXTURE_IDLE_FRAMES {
                surfaces.finish_frame();
            }
            assert!(!surfaces.streams.contains(stream));
            assert!(!surfaces.shared_textures.contains(shared.handle.0 as usize));
            Ok(())
        }
    }
}
