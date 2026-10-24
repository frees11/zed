//! Draws [`ExternalSurface::D3D11Shared`] frames: NV12 textures that a decoder on another
//! Direct3D 11 device shares through an NT handle.
//!
//! Each shared texture is opened once with `OpenSharedResource1` and cached by its NT handle.
//! For every new generation of a stream the renderer takes the frame's keyed mutex for at most
//! [`KEYED_MUTEX_TIMEOUT_MS`], copies the frame into an NV12 texture of its own and hands the
//! shared texture straight back. Redraws of a generation sample that copy, so they never wait
//! on the producer, and a frame that cannot be copied leaves the stream's previous frame on
//! screen, logged once until the stream copies again.
//!
//! The opened shared textures and the stream copies belong to the device, not to a window:
//! a keyed-mutex frame can be taken only once, so every window on the device draws a
//! generation from the copy the first of them made. Shared textures, and copies of frames that
//! can be copied again, are released once no window used them for
//! [`EXTERNAL_TEXTURE_IDLE_FRAMES`] rendered frames. The copy of a keyed-mutex frame is the
//! only one of its generation, so up to [`MAX_RETAINED_KEYED_COPIES`] idle ones stay; a stream
//! whose copy had to go draws nothing until its next generation instead of waiting on a mutex
//! the producer will not release for that frame again. A lost device drops the component of
//! every window, and the caches go with the old device.

use std::{
    cell::RefCell,
    cmp::Reverse,
    collections::hash_map::Entry,
    ffi::c_void,
    hash::Hash,
    num::NonZeroU32,
    rc::{Rc, Weak},
    slice,
};

use anyhow::{Context as _, Result};
use collections::FxHashMap;
use gpui::{
    Bounds, D3D11SharedFrame, ExternalStreamId, ExternalSurface, KeyedMutexKeys,
    PaintExternalSurface, ScaledPixels, UnsupportedExternalSurfaces, YuvColorMatrix,
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

/// Idle copies of keyed-mutex frames a device keeps beyond [`EXTERNAL_TEXTURE_IDLE_FRAMES`]: such
/// a frame cannot be copied again, so a stream shown again unchanged draws from its old copy.
pub(crate) const MAX_RETAINED_KEYED_COPIES: usize = 8;

/// Streams whose keyed-mutex copy was released that a device remembers, so that repainting the
/// released generation does not wait on a mutex the producer will not release for it again.
const MAX_RELEASED_KEYED_GENERATIONS: usize = 1024;

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
        self.release_idle_retaining(frame, |_| false, 0);
    }

    /// Releases every value that no frame used for [`EXTERNAL_TEXTURE_IDLE_FRAMES`] frames up
    /// to and including `frame`, except the `retain_limit` most recently used idle values that
    /// `retain` selects. Returns the released values.
    fn release_idle_retaining(
        &mut self,
        frame: u64,
        retain: impl Fn(&V) -> bool,
        retain_limit: usize,
    ) -> Vec<(K, V)> {
        let is_idle =
            |entry: &IdleEntry<V>| frame - entry.last_used_frame >= EXTERNAL_TEXTURE_IDLE_FRAMES;
        let mut retained: Vec<(K, u64)> = self
            .entries
            .iter()
            .filter(|(_, entry)| is_idle(entry) && retain(&entry.value))
            .map(|(key, entry)| (*key, entry.last_used_frame))
            .collect();
        retained.sort_by_key(|(_, last_used_frame)| Reverse(*last_used_frame));
        retained.truncate(retain_limit);
        let released: Vec<K> = self
            .entries
            .iter()
            .filter(|(key, entry)| {
                is_idle(entry) && !retained.iter().any(|(retained, _)| retained == *key)
            })
            .map(|(key, _)| *key)
            .collect();
        released
            .into_iter()
            .filter_map(|key| self.remove(key).map(|value| (key, value)))
            .collect()
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
    /// Whether the frame was taken under a keyed mutex, so that this is its only copy.
    keyed: bool,
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

/// The keyed mutex `frame` is copied under, with the frame's keys. A device that opens a
/// keyed-mutex texture must acquire the mutex before it reads the texture, so a frame that
/// disagrees with its texture about the mutex is an error.
fn frame_keyed_mutex<'a, M>(
    frame: &D3D11SharedFrame,
    mutex: Option<&'a M>,
) -> Result<Option<(KeyedMutexKeys, &'a M)>> {
    match (frame.keyed_mutex, mutex) {
        (Some(keys), Some(mutex)) => Ok(Some((keys, mutex))),
        (None, None) => Ok(None),
        (Some(_), None) => Err(anyhow::anyhow!(
            "the frame names keyed mutex keys, shared texture handle {:#x} has no keyed mutex",
            frame.nt_handle
        )),
        (None, Some(_)) => Err(anyhow::anyhow!(
            "shared texture handle {:#x} has a keyed mutex, the frame names no keyed mutex keys",
            frame.nt_handle
        )),
    }
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

    let Some((keys, mutex)) = frame_keyed_mutex(frame, shared.keyed_mutex.as_ref())? else {
        copy_region();
        return Ok(FrameCopy::Copied);
    };
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

/// Hands every renderer on the same device the same value while any of them holds it.
struct PerDevice<T> {
    values: RefCell<Vec<Weak<RefCell<T>>>>,
}

impl<T> PerDevice<T> {
    const fn new() -> Self {
        Self {
            values: RefCell::new(Vec::new()),
        }
    }

    /// The live value `belongs` accepts, or a new one from `create`.
    fn get_or_create(
        &self,
        belongs: impl Fn(&T) -> bool,
        create: impl FnOnce() -> Result<T>,
    ) -> Result<Rc<RefCell<T>>> {
        let mut values = self.values.borrow_mut();
        values.retain(|value| value.strong_count() > 0);
        if let Some(value) = values
            .iter()
            .filter_map(Weak::upgrade)
            .find(|value| belongs(&value.borrow()))
        {
            return Ok(value);
        }
        let value = Rc::new(RefCell::new(create()?));
        values.push(Rc::downgrade(&value));
        Ok(value)
    }
}

thread_local! {
    static DEVICE_FRAME_COPIES: PerDevice<FrameCopies> = const { PerDevice::new() };
}

/// The textures every window on one device draws external surfaces from.
struct FrameCopies {
    device: ID3D11Device,
    device1: ID3D11Device1,
    shared_textures: IdleCache<usize, SharedTexture>,
    streams: IdleCache<ExternalStreamId, StreamCopy>,
    /// The generation of each stream whose keyed-mutex copy was released while idle.
    released_keyed_generations: IdleCache<ExternalStreamId, u64>,
    failing_streams: IdleCache<ExternalStreamId, ()>,
    /// Frames rendered by any window on the device.
    frame: u64,
}

impl FrameCopies {
    fn for_device(device: &ID3D11Device) -> Result<Rc<RefCell<Self>>> {
        DEVICE_FRAME_COPIES.with(|copies| {
            copies.get_or_create(|copies| copies.device == *device, || Self::new(device))
        })
    }

    fn new(device: &ID3D11Device) -> Result<Self> {
        Ok(Self {
            device: device.clone(),
            device1: device
                .cast()
                .context("the Direct3D 11.1 device interface opens shared textures")?,
            shared_textures: IdleCache::default(),
            streams: IdleCache::default(),
            released_keyed_generations: IdleCache::default(),
            failing_streams: IdleCache::default(),
            frame: 0,
        })
    }

    /// Copies `frame` unless its generation already has a copy on the device. A frame that
    /// cannot be copied is logged once until its stream copies again.
    fn prepare(&mut self, context: &ID3D11DeviceContext, frame: &D3D11SharedFrame) {
        let uploaded = self
            .streams
            .get(frame.stream, self.frame)
            .and_then(|copy| copy.frame)
            .is_some_and(|copied| copied.generation == frame.generation);
        if uploaded {
            return;
        }
        let released = self
            .released_keyed_generations
            .get(frame.stream, self.frame)
            .is_some_and(|generation| *generation == frame.generation);
        if released {
            self.report_failure(
                frame.stream,
                &anyhow::anyhow!(
                    "the copy of generation {} was released while the stream was hidden, and a keyed mutex frame cannot be taken twice; waiting for the next generation",
                    frame.generation
                ),
            );
            return;
        }
        match self.upload(context, frame) {
            Ok(()) => {
                self.released_keyed_generations.remove(frame.stream);
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

    /// Ends a frame rendered by any window on the device: releases the textures no window used
    /// for [`EXTERNAL_TEXTURE_IDLE_FRAMES`] frames, except the retained keyed-mutex copies.
    fn finish_frame(&mut self) {
        self.shared_textures.release_idle(self.frame);
        let released = self.streams.release_idle_retaining(
            self.frame,
            |copy| copy.frame.is_some_and(|copied| copied.keyed),
            MAX_RETAINED_KEYED_COPIES,
        );
        for (stream, copy) in released {
            if let Some(copied) = copy.frame.filter(|copied| copied.keyed) {
                self.released_keyed_generations
                    .insert(stream, copied.generation, self.frame);
            }
        }
        self.released_keyed_generations.release_idle_retaining(
            self.frame,
            |_| true,
            MAX_RELEASED_KEYED_GENERATIONS,
        );
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
                    keyed: frame.keyed_mutex.is_some(),
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

/// Draws the external surfaces of a window's scenes from the textures of its device.
pub(super) struct ExternalSurfaces {
    pipeline: Nv12Pipeline,
    copies: Rc<RefCell<FrameCopies>>,
    unsupported: UnsupportedExternalSurfaces,
}

impl ExternalSurfaces {
    pub(super) fn new(device: &ID3D11Device) -> Result<Self> {
        Ok(Self {
            pipeline: Nv12Pipeline::new(device).context("creating the NV12 surface pipeline")?,
            copies: FrameCopies::for_device(device)?,
            unsupported: UnsupportedExternalSurfaces::default(),
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
        let mut copies = self.copies.borrow_mut();
        for painted in surfaces {
            let ExternalSurface::D3D11Shared(frame) = &painted.surface else {
                self.unsupported.skip(RENDERER_NAME, painted.surface.kind());
                continue;
            };
            copies.prepare(context, frame);
            let frame_index = copies.frame;
            let Some(copy) = copies.streams.get(frame.stream, frame_index) else {
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

    /// Ends a rendered frame: releases the textures no window on the device used for
    /// [`EXTERNAL_TEXTURE_IDLE_FRAMES`] frames.
    pub(super) fn finish_frame(&mut self) {
        self.copies.borrow_mut().finish_frame();
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
    fn retained_idle_entries_stay_and_the_rest_go() {
        let mut cache = IdleCache::default();
        cache.insert(1, "keyed copy", 0);
        cache.insert(2, "plain copy", 0);
        let after_idle = EXTERNAL_TEXTURE_IDLE_FRAMES * 10;
        let released = cache.release_idle_retaining(after_idle, |copy| *copy == "keyed copy", 1);
        assert_eq!(released, vec![(2, "plain copy")]);
        assert!(cache.contains(1));
        assert_eq!(cache.get(1, after_idle).copied(), Some("keyed copy"));
    }

    #[test]
    fn only_the_most_recently_used_retained_entries_stay() {
        let mut cache = IdleCache::default();
        for (key, last_used_frame) in [(1, 5), (2, 9), (3, 7)] {
            cache.insert(key, "keyed copy", last_used_frame);
        }
        cache.insert(4, "keyed copy", EXTERNAL_TEXTURE_IDLE_FRAMES + 9);
        let mut released =
            cache.release_idle_retaining(EXTERNAL_TEXTURE_IDLE_FRAMES + 10, |_| true, 2);
        released.sort();
        assert_eq!(released, vec![(1, "keyed copy")]);
        for key in [2, 3, 4] {
            assert!(cache.contains(key), "entry {key}");
        }
    }

    #[test]
    fn retaining_keeps_nothing_that_is_not_idle_out_of_the_limit() {
        let mut cache = IdleCache::default();
        cache.insert(1, "keyed copy", 0);
        cache.insert(2, "keyed copy", 100);
        let released = cache.release_idle_retaining(EXTERNAL_TEXTURE_IDLE_FRAMES, |_| true, 0);
        assert_eq!(released, vec![(1, "keyed copy")]);
        assert!(cache.contains(2), "an entry used 20 frames ago is not idle");
    }

    #[test]
    fn renderers_on_one_device_share_one_value() -> Result<()> {
        let per_device = PerDevice::new();
        let mut created = 0;
        let mut get = |device: u32| {
            per_device.get_or_create(
                |value: &(u32, u32)| value.0 == device,
                || {
                    created += 1;
                    Ok((device, created))
                },
            )
        };
        let first_window = get(1)?;
        let second_window = get(1)?;
        assert!(Rc::ptr_eq(&first_window, &second_window));
        let other_device = get(2)?;
        assert!(!Rc::ptr_eq(&first_window, &other_device));
        drop((first_window, second_window));
        let after_every_window_closed = get(1)?;
        assert_eq!(*after_every_window_closed.borrow(), (1, 3));
        Ok(())
    }

    #[test]
    fn a_failed_creation_shares_nothing() {
        let per_device = PerDevice::<u32>::new();
        let result = per_device.get_or_create(|_| true, || anyhow::bail!("no device"));
        assert!(result.is_err());
        assert!(per_device.values.borrow().is_empty());
    }

    #[test]
    fn frames_must_agree_with_their_texture_about_the_keyed_mutex() {
        let keys = gpui::KeyedMutexKeys {
            acquire: 1,
            release: 0,
        };
        let texture_mutex = "keyed mutex";
        let frame = |keyed_mutex| D3D11SharedFrame {
            keyed_mutex,
            nt_handle: 0x40,
            ..shared_frame(16, 16, YuvColorMatrix::Bt601Full)
        };

        let keyed = frame(Some(keys));
        let taken = frame_keyed_mutex(&keyed, Some(&texture_mutex))
            .expect("a keyed frame of a keyed texture is copied under the mutex");
        assert_eq!(taken, Some((keys, &texture_mutex)));
        let plain = frame(None);
        assert_eq!(frame_keyed_mutex::<&str>(&plain, None).ok(), Some(None));

        let error = frame_keyed_mutex(&plain, Some(&texture_mutex))
            .expect_err("a keyed texture must not be read without its mutex");
        assert_eq!(
            error.to_string(),
            "shared texture handle 0x40 has a keyed mutex, the frame names no keyed mutex keys"
        );
        let error = frame_keyed_mutex::<&str>(&keyed, None)
            .expect_err("keys for a texture without a keyed mutex");
        assert_eq!(
            error.to_string(),
            "the frame names keyed mutex keys, shared texture handle 0x40 has no keyed mutex"
        );
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
            keyed: false,
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
            keyed: false,
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
        /// guarded by a keyed mutex unless made by [`SharedNv12::without_keyed_mutex`].
        struct SharedNv12 {
            _texture: ID3D11Texture2D,
            mutex: Option<IDXGIKeyedMutex>,
            handle: Owned<HANDLE>,
        }

        impl SharedNv12 {
            fn new(producer: &Warp, samples: (u8, u8, u8)) -> Result<Self> {
                Self::create(
                    producer,
                    samples,
                    D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32,
                )
            }

            fn without_keyed_mutex(producer: &Warp, samples: (u8, u8, u8)) -> Result<Self> {
                Self::create(producer, samples, 0)
            }

            fn create(
                producer: &Warp,
                (y, cb, cr): (u8, u8, u8),
                keyed_mutex_flag: u32,
            ) -> Result<Self> {
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
                    MiscFlags: D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 as u32 | keyed_mutex_flag,
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
                    mutex: texture.cast().ok(),
                    _texture: texture,
                    handle: unsafe { Owned::new(handle) },
                })
            }

            /// Hands the frame to the renderer the way a decoder does after writing it.
            fn release_frame(&self) -> Result<()> {
                let mutex = self
                    .mutex
                    .as_ref()
                    .context("the texture has no keyed mutex")?;
                let wait = acquire_keyed_mutex(mutex, PRODUCER_KEY, PRODUCER_TIMEOUT_MS);
                anyhow::ensure!(
                    wait == KeyedMutexWait::Acquired,
                    "producer could not take key {PRODUCER_KEY} back: {wait:?}"
                );
                unsafe { mutex.ReleaseSync(FRAME_READY_KEY) }?;
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
            let mut copies = surfaces.copies.borrow_mut();
            let frame = copies.frame;
            copies
                .streams
                .get(stream, frame)
                .and_then(|copy| copy.frame)
                .map(|copied| copied.generation)
        }

        fn is_failing(surfaces: &ExternalSurfaces, stream: ExternalStreamId) -> bool {
            surfaces.copies.borrow().failing_streams.contains(stream)
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
            assert_eq!(surfaces.copies.borrow().shared_textures.entries.len(), 1);
            Ok(())
        }

        #[test]
        #[ignore = "hw"]
        fn warp_releases_idle_copies_without_keyed_mutex_and_copies_them_again() -> Result<()> {
            let renderer = Warp::new()?;
            let producer = Warp::new()?;
            let target = Target::new(&renderer)?;
            let mut surfaces = ExternalSurfaces::new(&renderer.device)?;
            let shared = SharedNv12::without_keyed_mutex(&producer, (128, 128, 128))?;
            let stream = ExternalStreamId::new();
            let frame = D3D11SharedFrame {
                keyed_mutex: None,
                ..shared.frame(stream, 1, YuvColorMatrix::Bt601Full)
            };
            target.render(&renderer, &mut surfaces, frame)?;
            assert!(surfaces.copies.borrow().streams.contains(stream));
            for _ in 0..EXTERNAL_TEXTURE_IDLE_FRAMES {
                surfaces.finish_frame();
            }
            {
                let copies = surfaces.copies.borrow();
                assert!(!copies.streams.contains(stream));
                assert!(!copies.shared_textures.contains(shared.handle.0 as usize));
                assert!(!copies.released_keyed_generations.contains(stream));
            }

            target.render(&renderer, &mut surfaces, frame)?;
            assert_eq!(
                copied_generation(&mut surfaces, stream),
                Some(1),
                "a frame without a keyed mutex is copied again from its shared texture"
            );
            assert!(!is_failing(&surfaces, stream));
            Ok(())
        }

        #[test]
        #[ignore = "hw"]
        fn warp_keeps_an_idle_keyed_copy_and_releases_its_shared_texture() -> Result<()> {
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
            assert_eq!(
                copied_generation(&mut surfaces, stream),
                Some(1),
                "the only copy of a keyed-mutex frame outlives the idle limit"
            );
            let copies = surfaces.copies.borrow();
            assert!(!copies.shared_textures.contains(shared.handle.0 as usize));
            assert!(!copies.released_keyed_generations.contains(stream));
            Ok(())
        }

        #[test]
        #[ignore = "hw"]
        fn warp_repaints_a_keyed_generation_hidden_past_the_idle_limit_from_its_copy() -> Result<()>
        {
            let renderer = Warp::new()?;
            let producer = Warp::new()?;
            let target = Target::new(&renderer)?;
            let mut surfaces = ExternalSurfaces::new(&renderer.device)?;
            let shared = SharedNv12::new(&producer, (128, 128, 128))?;
            let stream = ExternalStreamId::new();
            let frame = shared.frame(stream, 1, YuvColorMatrix::Bt601Full);
            shared.release_frame()?;
            target.render(&renderer, &mut surfaces, frame)?;
            for _ in 0..2 * EXTERNAL_TEXTURE_IDLE_FRAMES {
                surfaces.finish_frame();
            }

            let image = target.render(&renderer, &mut surfaces, frame)?;
            assert_every_pixel(&image, GREY, BT601_FULL_TOLERANCE);
            assert!(
                !is_failing(&surfaces, stream),
                "the repaint tried to take the keyed mutex again"
            );
            Ok(())
        }

        #[test]
        #[ignore = "hw"]
        fn warp_keyed_copies_beyond_the_retained_limit_wait_for_a_new_generation() -> Result<()> {
            let renderer = Warp::new()?;
            let producer = Warp::new()?;
            let target = Target::new(&renderer)?;
            let mut surfaces = ExternalSurfaces::new(&renderer.device)?;
            let frames = (0..=MAX_RETAINED_KEYED_COPIES)
                .map(|_| {
                    let shared = SharedNv12::new(&producer, (128, 128, 128))?;
                    shared.release_frame()?;
                    let frame = shared.frame(ExternalStreamId::new(), 1, YuvColorMatrix::Bt601Full);
                    Ok((shared, frame))
                })
                .collect::<Result<Vec<_>>>()?;
            for (_, frame) in &frames {
                target.render(&renderer, &mut surfaces, *frame)?;
            }
            for _ in 0..EXTERNAL_TEXTURE_IDLE_FRAMES {
                surfaces.finish_frame();
            }

            let (oldest_texture, oldest) = &frames[0];
            let started = std::time::Instant::now();
            surfaces.draw(&renderer.context, &[painted(*oldest, SIDE as f32)])?;
            assert!(
                started.elapsed() < std::time::Duration::from_millis(KEYED_MUTEX_TIMEOUT_MS.into()),
                "the repaint of a released keyed generation waited on the keyed mutex"
            );
            surfaces.finish_frame();
            let image = target.render(&renderer, &mut surfaces, *oldest)?;
            assert_every_pixel(&image, BLACK, 0);
            assert!(is_failing(&surfaces, oldest.stream));
            for (_, frame) in &frames[1..] {
                let image = target.render(&renderer, &mut surfaces, *frame)?;
                assert_every_pixel(&image, GREY, BT601_FULL_TOLERANCE);
            }

            oldest_texture.release_frame()?;
            let next = D3D11SharedFrame {
                generation: 2,
                ..*oldest
            };
            let image = target.render(&renderer, &mut surfaces, next)?;
            assert_every_pixel(&image, GREY, BT601_FULL_TOLERANCE);
            assert!(!is_failing(&surfaces, oldest.stream));
            Ok(())
        }

        #[test]
        #[ignore = "hw"]
        fn warp_windows_on_one_device_draw_a_keyed_frame_from_one_copy() -> Result<()> {
            let renderer = Warp::new()?;
            let producer = Warp::new()?;
            let main_window = Target::new(&renderer)?;
            let pop_out_window = Target::new(&renderer)?;
            let mut main_surfaces = ExternalSurfaces::new(&renderer.device)?;
            let mut pop_out_surfaces = ExternalSurfaces::new(&renderer.device)?;
            let shared = SharedNv12::new(&producer, (128, 128, 128))?;
            let stream = ExternalStreamId::new();
            let frame = shared.frame(stream, 1, YuvColorMatrix::Bt601Full);
            shared.release_frame()?;

            for _ in 0..3 {
                for (target, surfaces) in [
                    (&main_window, &mut main_surfaces),
                    (&pop_out_window, &mut pop_out_surfaces),
                ] {
                    let image = target.render(&renderer, surfaces, frame)?;
                    assert_every_pixel(&image, GREY, BT601_FULL_TOLERANCE);
                }
            }
            assert!(Rc::ptr_eq(&main_surfaces.copies, &pop_out_surfaces.copies));
            assert!(
                !is_failing(&pop_out_surfaces, stream),
                "a window waited on the keyed mutex the other window already took"
            );
            assert_eq!(
                main_surfaces.copies.borrow().shared_textures.entries.len(),
                1
            );
            Ok(())
        }

        #[test]
        #[ignore = "hw"]
        fn warp_frames_that_disagree_with_their_texture_about_the_keyed_mutex_are_not_drawn()
        -> Result<()> {
            let renderer = Warp::new()?;
            let producer = Warp::new()?;
            let target = Target::new(&renderer)?;
            let mut surfaces = ExternalSurfaces::new(&renderer.device)?;

            let keyed = SharedNv12::new(&producer, (255, 128, 128))?;
            let without_keys = D3D11SharedFrame {
                keyed_mutex: None,
                ..keyed.frame(ExternalStreamId::new(), 1, YuvColorMatrix::Bt601Full)
            };
            let image = target.render(&renderer, &mut surfaces, without_keys)?;
            assert_every_pixel(&image, BLACK, 0);
            assert!(is_failing(&surfaces, without_keys.stream));

            let plain = SharedNv12::without_keyed_mutex(&producer, (255, 128, 128))?;
            let with_keys = plain.frame(ExternalStreamId::new(), 1, YuvColorMatrix::Bt601Full);
            let image = target.render(&renderer, &mut surfaces, with_keys)?;
            assert_every_pixel(&image, BLACK, 0);
            assert!(is_failing(&surfaces, with_keys.stream));
            Ok(())
        }
    }
}
