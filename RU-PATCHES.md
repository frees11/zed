# Remote Ultra GPUI patches

Remote Ultra builds on GPUI from this fork of zed.

- `ru/base` is the exact upstream pin `9f305aaf88370e8ca39c702242186e3d8f97ecca` (`gpui_macos: Handle null open and save panels (#66042)`).
- `ru/patches` is `ru/base` plus one commit per patch, subject `[ru-Fn] <crate>[, <crate>]...: <what>`, followed by this file as its own `[ru]` housekeeping commit. F1 to F6 and F10 started as a `cherry-pick -x` of the branch they were developed on, so their message names that source commit; the fixes of the adversarial review of 2026-10-25 are folded into each patch. F8 was written on `ru/patches`.
- Tags `ru-YYYYMMDD-n` mark the `ru/patches` commits the Remote Ultra workspace pins.
- `tools/check-zed-patches.sh [--target <target>]... <zed-tree>` in the Remote Ultra repository checks every crate the patches touch on macOS, `aarch64-apple-ios-sim`, `aarch64-apple-ios`, `x86_64-pc-windows-gnu` and `aarch64-linux-android` (dev and release), then runs each patch commit's `Ru-Test: <host|x86_64-pc-windows-gnu|linux> <cargo test args>` trailers: `x86_64-pc-windows-gnu` under wine64, `linux` in the Linux dev container of `tools/linux/docker.sh` (llvmpipe/lavapipe Vulkan). `[ru]` commits are not built or tested, and the check fails when one changes anything but this file. `--target` limits both phases, for example to clean the target directory between targets. `tools/zed-rebase.sh` moves the patches to a new upstream pin and carries the `[ru]` commits along; when upstream has merged (or `--drop` names) every patch, it finishes with only them.

Test commands below run from the root of this tree with the Remote Ultra toolchain (`cargo +1.99.0`). Without Xcode's Metal Toolchain, `gpui_apple` cannot compile `shaders.metal` at build time, so every macOS or iOS build that reaches `gpui_apple` (including the `gpui` tests, through `gpui_platform`) needs its `runtime_shaders` feature. `gpui_apple`'s build script is compiled with the host's cfg, so from a macOS host it also tries to compile Metal shaders for Windows and Android targets and stops with `unsupported Metal shader target` unless `runtime_shaders` is on. The patched crates check on every target with:

```
cargo check --locked --profile <dev|release> [--target <triple>] -p gpui -p gpui_apple -p gpui_ios -p gpui_linux -p gpui_macos -p gpui_web -p gpui_wgpu -p gpui_windows --features gpui_apple/runtime_shaders
```

(Android: `cargo ndk -t arm64-v8a -P 30 check …` with the same arguments.) `check-zed-patches.sh` adds `--features gpui_apple/runtime_shaders` whenever `gpui_apple` is among the patched crates. Tests that create a Metal device need a process that may use the GPU (outside a sandbox); wine and Docker need it too. The `linux` tests run as

```
tools/linux/docker.sh env RUSTUP_TOOLCHAIN=1.99.0 CARGO_TARGET_DIR=/tmp/zed-patches-target CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-fuse-ld=bfd" cargo test --locked <args>
```

from this tree inside the Remote Ultra checkout; the container has no `lld`, which this tree's `.cargo/config.toml` asks for on `aarch64-unknown-linux-gnu`.

| Patch | Crates | Upstream |
|---|---|---|
| F1 | gpui_windows | not upstream; draft PR candidate |
| F2 | gpui (+ skip arms in gpui_apple, gpui_wgpu, gpui_windows) | not upstream; draft PR candidate together with F3 |
| F3 | gpui_wgpu | not upstream; draft PR candidate together with F2 |
| F4 | gpui_windows | not upstream; draft PR candidate after F1 and F2 |
| F5 | gpui_apple | lifetime part is upstream PR #65563 (open, draft); the 420v/420f BT.601/BT.709 part is a draft PR candidate |
| F6 | gpui_ios | not upstream; draft PR candidate |
| F8 | gpui, gpui_linux (+ the variant's arms in gpui_macos, gpui_windows, gpui_web) | not upstream; reverts the removal of `CursorStyle::None` in #55664 with persistent hiding; fork only |
| F10 | gpui_ios | not upstream; fork only |

Draft PRs are opened only with the owner's approval and a signed CLA.

## F1 gpui_windows: compile HLSL at runtime when the target is Windows but the host is not

`build.rs` decides from `CARGO_CFG_TARGET_OS` and `CARGO_CFG_DEBUG_ASSERTIONS` instead of its own cfg. Only a release build for Windows on a Windows host runs `fxc` and sets `cfg(gpui_windows_precompiled_shaders)`. Every other Windows build (debug, or cross-compiled from macOS or Linux) compiles the embedded HLSL with `D3DCompile` from `include_str!` at runtime, with `alpha_correction.hlsl` inlined in Rust because `D3DCompile` gets no include handler; `#line` directives around the inlined file keep compiler messages on their own file and line. Release builds use `D3DCOMPILE_OPTIMIZATION_LEVEL3`, debug builds `D3DCOMPILE_DEBUG | D3DCOMPILE_SKIP_OPTIMIZATION`. A cross-compiled exe therefore needs `d3dcompiler_47.dll` on the target machine.

Files:
- `crates/gpui_windows/build.rs`
- `crates/gpui_windows/src/directx_renderer.rs`

Tests (`directx_renderer::shader_resources::tests`):
- `embedded_shader_sources_inline_alpha_correction` and `embedded_shader_lines_report_their_own_file_and_line` (every line of the expanded source resolves through the `#line` directives to the same line of its own file) run under wine64 (`Ru-Test` trailer):
  `cargo test --target x86_64-pc-windows-gnu -p gpui_windows --features test-support --lib -- directx_renderer::shader_resources::tests::embedded_shader_` with `CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine64`.
- `every_shader_module_yields_dxbc_bytecode` needs Microsoft's `d3dcompiler_47.dll`; wine's vkd3d HLSL parser rejects `shaders.hlsl`, so it passes only on Windows.
- Plan acceptance: a release `remote-ultra.exe` built on macOS renders on a Windows PC.

Upstream: not upstream. Draft PR candidate.

## F2 gpui: SurfaceSource::External and Window::paint_external_surface

Adds video frames decoded outside GPUI as a scene primitive. `ExternalSurface` is either a `CpuNv12Frame` (NV12 planes in `PlaneBuffer`s with an `Nv12Layout` that checks strides and plane sizes) or a `D3D11SharedFrame` (NT handle of a shared texture plus keyed mutex keys). Every frame carries an `ExternalStreamId`, a generation and a `YuvColorMatrix` (BT.601 full, BT.709 limited, BT.709 full) whose `ycbcr_to_rgb` matrix the renderers upload. The `surface` element paints a `SurfaceSource::External` fitted into its bounds by its `ObjectFit`; `Window::paint_external_surface` inserts a `PaintExternalSurface` into the scene, which batches it as `PrimitiveBatch::ExternalSurfaces`. Renderers that cannot draw a kind skip it through `UnsupportedExternalSurfaces`, which logs once per renderer and kind; this commit adds that skip arm to the Metal, wgpu and DirectX renderers. `KeyedMutexKeys` documents the producer contract: a renderer takes the mutex once per generation and draws later paints of it, in any window, from its own copy. It keeps the copies of generations still painted and only a limited number of idle ones (DirectX: 8 unused for 120 frames); a repaint of a generation whose copy went draws nothing, without waiting on the mutex, until the stream paints a new generation. `keyed_mutex` is set exactly when the texture has a keyed mutex.

Files:
- `crates/gpui/src/external_surface.rs` (new)
- `crates/gpui/src/elements/surface.rs`
- `crates/gpui/src/gpui.rs`
- `crates/gpui/src/scene.rs`
- `crates/gpui/src/window.rs`
- `crates/gpui_apple/src/metal_renderer.rs`
- `crates/gpui_wgpu/src/wgpu_renderer.rs`
- `crates/gpui_windows/src/directx_renderer.rs`

Tests:
- `gpui::external_surface::tests`: `full_range_grey_keeps_its_luma`, `limited_range_stretches_luma_16_to_235`, `each_matrix_decodes_its_own_red`, `bt601_and_bt709_disagree_on_chroma`, `packed_layout_rounds_odd_chroma_up`, `frame_accepts_planes_without_trailing_row_padding`, `frame_rejects_short_planes_and_strides`, `frames_compare_by_stream_and_generation`, `surfaces_can_move_from_a_decoder_thread`, `stream_ids_are_unique`, `unsupported_kinds_are_reported_once_each`.
- `gpui::scene::tests`: `external_surfaces_batch_between_the_primitives_they_overlap`, `clear_drops_external_surfaces`.
- `gpui::elements::surface` tests: `external_surface_reaches_the_scene_letterboxed`, `each_frame_paints_the_generation_it_was_given`.
- `gpui_apple::metal_renderer::tests::external_surfaces_are_skipped_without_drawing` (Metal device).
- Commands (`Ru-Test` trailers):
  `cargo test -p gpui --features gpui_platform/runtime_shaders --lib -- external_surface::tests scene::tests elements::surface::tests`
  `cargo test -p gpui_apple --features runtime_shaders --lib -- metal_renderer::tests::external_surfaces_are_skipped_without_drawing`

Upstream: not upstream. Draft PR candidate, one PR with F3.

## F3 gpui_wgpu: draw CPU NV12 external surfaces

Draws `CpuNv12Frame`s with an R8 luma and an RG8 chroma texture per stream. Textures are uploaded only when a stream shows a new generation, rebuilt when its size changes and evicted after 120 rendered frames without use. The shader converts with a per-surface uniform holding the frame's `YuvColorMatrix::ycbcr_to_rgb`; the uniforms of a frame's surfaces sit in slots of one persistent buffer (each slot at the device's uniform offset alignment, one `write_buffer` per frame, grown only for more surfaces than any frame before) instead of a buffer allocated per surface and frame. The recovery after repeated GPU errors also drops the stream textures, so one a failed allocation left invalid is rebuilt instead of blanking every later frame. Shared D3D11 frames and frames above the device's texture limit are skipped with a one-time log.

Files:
- `crates/gpui_wgpu/src/shaders.wgsl`
- `crates/gpui_wgpu/src/wgpu_renderer.rs`

Tests (`gpui_wgpu::wgpu_renderer::tests`):
- Headless readback, `#[cfg(target_os = "linux")]` (a Vulkan adapter such as lavapipe): `nv12_bt601_full_grey_is_within_two_levels`, `nv12_bt709_limited_grey_is_within_three_levels`, `nv12_bt709_limited_stretches_16_and_235_to_black_and_white`, `nv12_each_matrix_decodes_its_own_red`, `nv12_uploads_only_a_new_generation`, `nv12_textures_are_evicted_after_120_idle_frames`, `nv12_drawn_streams_keep_their_textures`, `nv12_size_change_rebuilds_the_stream_textures`, `nv12_honours_strides_and_bounds`, `nv12_odd_dimensions_render`, `nv12_frames_above_the_texture_limit_are_skipped`, `nv12_frames_reuse_one_params_buffer`, `nv12_surfaces_of_one_frame_keep_their_own_params`, `nv12_streams_are_rebuilt_after_gpu_error_recovery`, `d3d11_shared_surfaces_are_skipped`.
- No GPU, every host: `surface_params_match_the_wgsl_uniform_layout`, `surface_params_slots_start_at_the_uniform_offset_alignment`.
- Commands (`Ru-Test` trailers): `host -p gpui_wgpu --lib -- wgpu_renderer::tests::surface_params_` and `linux -p gpui_wgpu --lib -- wgpu_renderer::tests::nv12_ wgpu_renderer::tests::d3d11_shared_surfaces_are_skipped wgpu_renderer::tests::surface_params_`. On 2026-10-25 all 15 readback tests passed in the arm64 Linux dev container on llvmpipe (Mesa lavapipe, LLVM 20.1.2), with the 601 full grey within 2 and the 709 limited grey within 3 levels.

Upstream: not upstream. Draft PR candidate, one PR with F2.

## F4 gpui_windows: draw shared D3D11 NV12 external surfaces

`DirectXRenderer` draws `ExternalSurface::D3D11Shared` frames. A shared texture is opened once with `OpenSharedResource1` and cached by NT handle; a duplicated handle and `CompareObjectHandles` detect a handle value the producer reused. For each new generation the renderer takes the keyed mutex for at most 4 ms (from the raw `AcquireSync` result), copies the frame into its own NV12 texture and releases with the frame's release key, so redraws never wait on the producer. A frame that cannot be copied keeps the previous frame on screen with a one-time log, an abandoned mutex drops the shared texture, and a frame whose keys disagree with its texture's keyed mutex (keys without a mutex, or a mutex without keys) is not copied. The caches belong to the device and are shared by every window on it (`PerDevice`), so a keyed-mutex frame painted in two windows is taken once. Shared textures and copies of frames without a keyed mutex unused for 120 rendered frames of any window are released; a keyed-mutex copy is the only one of its generation, so up to 8 idle ones stay, and a stream whose copy had to go draws nothing until its next generation without waiting on the mutex. A lost device drops the caches. The `nv12_surface` HLSL shader samples R8 luma and R8G8 chroma views and converts with the same `ycbcr_to_rgb` matrix as gpui_wgpu; it is a module of the F1 runtime compile path and of the `fxc` build list. CPU NV12 frames are still skipped with a one-time log. `render_to_image`'s readback moved into `read_back_render_target`.

Files:
- `crates/gpui_windows/build.rs`
- `crates/gpui_windows/src/directx_renderer.rs`
- `crates/gpui_windows/src/directx_renderer/external_surfaces.rs` (new)
- `crates/gpui_windows/src/shaders.hlsl`

Tests (`directx_renderer::external_surfaces::tests`):
- Pure, under wine64 (`Ru-Test` trailer): `unused_entries_survive_119_frames_and_go_on_the_120th`, `entries_used_every_frame_stay`, `get_or_create_keeps_a_reusable_entry`, `get_or_create_replaces_an_entry_it_cannot_reuse`, `a_failed_replacement_leaves_no_stale_entry`, `retained_idle_entries_stay_and_the_rest_go`, `only_the_most_recently_used_retained_entries_stay`, `retaining_keeps_nothing_that_is_not_idle_out_of_the_limit`, `renderers_on_one_device_share_one_value`, `a_failed_creation_shares_nothing`, `frames_must_agree_with_their_texture_about_the_keyed_mutex`, `nv12_extent_rounds_odd_frames_up_to_even`, `nv12_extent_rejects_frames_above_the_texture_limit`, `subresources_count_every_mip_of_the_earlier_slices`, `keyed_mutex_results_map_to_waits`, `params_upload_the_wgpu_matrix_of_each_color`, `hlsl_reads_the_uploaded_matrix_as_each_colors_red_and_grey`, `params_match_the_hlsl_constant_buffer`, `params_carry_bounds_mask_and_uv_scale`.
  `cargo test --target x86_64-pc-windows-gnu -p gpui_windows --features test-support --lib -- directx_renderer::external_surfaces::tests` with `CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine64`.
- WARP pixel tests, `#[ignore = "hw"]` because wine has no WARP device; run on Windows with `-- --ignored`: `warp_draws_shared_nv12_frames_in_each_matrix`, `warp_redraws_a_generation_from_its_copy_and_hands_the_texture_back`, `warp_releases_idle_copies_without_keyed_mutex_and_copies_them_again`, `warp_keeps_an_idle_keyed_copy_and_releases_its_shared_texture`, `warp_repaints_a_keyed_generation_hidden_past_the_idle_limit_from_its_copy`, `warp_keyed_copies_beyond_the_retained_limit_wait_for_a_new_generation`, `warp_windows_on_one_device_draw_a_keyed_frame_from_one_copy`, `warp_frames_that_disagree_with_their_texture_about_the_keyed_mutex_are_not_drawn`. Not run yet: they need a Windows machine.

Upstream: not upstream. Draft PR candidate; it needs F1 (runtime shader path) and F2.

## F5 gpui_apple: keep video surfaces alive until the GPU has read them; decode 420v/420f with their BT.601/BT.709 matrix

Cherry-picks upstream zed PR #65563 (Davi Mello, commits `c77238f1a1` and `79c9daaed4`, without its README review marker): the command buffer's completed handler retains the `CVPixelBuffer` and its `CVMetalTexture`s, so a `CVPixelBufferPool` cannot recycle a surface the GPU still samples, and surfaces that cannot be drawn are logged once per renderer and skipped instead of asserting. On top of it, the surface fragment shader takes its YCbCr to RGB matrix as a fragment buffer (`SurfaceInputIndex::YCbCrToRgb`) built per surface by `SurfaceColor`: range from the pixel format (420v video, 420f full), luma weights from `kCVImageBufferYCbCrMatrixKey` (BT.601 or BT.709; untagged keeps BT.601), read with `CVBufferGetAttachment` because `CVBufferCopyAttachment` only exists from macOS 12 and binding it would abort GPUI on macOS 10.15 and 11. Other pixel formats and matrices are skipped through the same log-once path. Chroma zero is 128/255.

Files:
- `crates/gpui_apple/Cargo.toml` (dev-dependency feature `CVPixelBufferPool`)
- `crates/gpui_apple/src/gpui_apple.rs`
- `crates/gpui_apple/src/metal_renderer.rs`
- `crates/gpui_apple/src/shaders.metal`
- `crates/gpui_apple/src/surface_color.rs` (new)

Tests:
- `gpui_apple::surface_color::tests` (no GPU): `video_and_full_range_map_reference_black_and_white`, `video_range_decodes_published_colour_bar_codes`, `transform_inverts_the_itu_forward_conversion`, `untagged_full_range_keeps_the_previous_bt601_coefficients`, `range_follows_pixel_format_and_matrix_follows_attachment`, `other_pixel_formats_and_matrices_are_rejected`, `surfaces_read_the_matrix_attached_to_their_pixel_buffer`, `the_renderer_binds_no_core_video_function_newer_than_gpui_supports` (the test binary's Mach-O symbol table imports neither `_CVBufferCopyAttachment` nor `_CVBufferCopyAttachments`).
- `gpui_apple::metal_renderer::tests` (Metal device): `render_core_video_surface` (upstream, adapted), `surface_is_not_recycled_while_the_gpu_reads_it`, `dropping_renderer_releases_surface_after_gpu_completion`, `gpu_gate_is_released_when_assertions_unwind`, `surfaces_that_cannot_be_sampled_are_skipped`, `mixed_surface_batch_preserves_indices_and_clip`, `surface_errors_are_logged_once_per_renderer`, `surface_readback_follows_the_range_and_matrix_attachment` (420v/420f x 601/709/untagged within 3 code values), `surfaces_with_an_unsupported_matrix_are_skipped` (BT.2020), `pool_hands_out_none_of_20_surfaces_held_by_pending_frames`.
- Command (`Ru-Test` trailer): `cargo test -p gpui_apple --features runtime_shaders --lib -- surface_color::tests metal_renderer::tests::surface metal_renderer::tests::render_core_video_surface metal_renderer::tests::dropping_renderer_releases_surface_after_gpu_completion metal_renderer::tests::gpu_gate_is_released_when_assertions_unwind metal_renderer::tests::mixed_surface_batch_preserves_indices_and_clip metal_renderer::tests::pool_hands_out_none_of_20_surfaces_held_by_pending_frames`.

Upstream: the lifetime part is upstream PR https://github.com/zed-industries/zed/pull/65563, open as a draft and not merged on 2026-10-25; drop those hunks from F5 once a pin contains it. The 420v/420f BT.601/BT.709 part is ours and a draft PR candidate on top of #65563.

## F6 gpui_ios: deliver hardware keyboard presses as KeyDown, KeyUp and ModifiersChanged

`UIKey.keyCode` HID usages map to GPUI keystrokes. Main-block keys are named like gpui_macos names them, after the ASCII character they type without modifiers on the active layout (`UIKey.charactersIgnoringModifiers`), so AZERTY Cmd+Z is `cmd-z`; with shift a key that is not a letter is named after its shifted character and shift is cleared. Where the layout types no ASCII character on a key (Cyrillic, Thai) the key is named by its US ANSI position. Keypad and control keys have fixed names; `key_char` is the layout text from `UIKey.characters`. The Metal view becomes first responder whenever the text input view is not, so presses reach GPUI without a focused text field. Presses GPUI leaves unhandled, and every key up whose key down reached UIKit, go to `super`, so UIKit text input keeps working; key downs go straight to the input method while text is being composed. A key down GPUI handled never reaches UIKit's text input, which repeats only what it receives, so gpui_ios repeats it as `is_held` key downs (500 ms delay, 16 per second, the gpui_linux Wayland defaults) until its key up, another key down, a modifier change, the window turning inactive or a repeat GPUI no longer handles. Modifier keys are tracked per side and caps lock comes from the modifier flags.

Remote Ultra's iOS stream input reads `UIKey.keyCode` in its own `video_input_view` and does not depend on these names; its GPUI keystroke path (`gpui_key_to_hid`) maps layout names the same way it does for gpui_macos.

Files:
- `crates/gpui_ios/Cargo.toml` (`objc2-ui-kit` features `UIKey`, `UIKeyConstants`, `UIPress`, `UIPressesEvent`)
- `crates/gpui_ios/src/gpui_ios.rs`
- `crates/gpui_ios/src/ios/events.rs`
- `crates/gpui_ios/src/ios/platform.rs` (the window's repeat timer uses the platform executors)
- `crates/gpui_ios/src/ios/text_input.rs`
- `crates/gpui_ios/src/ios/window.rs`
- `crates/gpui_ios/src/keyboard.rs` (new; compiled for iOS and for tests on any host)

Tests (`gpui_ios::keyboard::tests`, run on the macOS host):
- `every_key_has_one_entry`, `names_match_ultra_keymap_us_ansi_table`, `shifted_symbols_match_ultra_keymap_and_clear_shift`, `letters_keep_shift_and_type_layout_text`, `keys_are_named_after_the_ascii_character_of_their_layout`, `layout_symbols_name_their_key_and_shift_names_the_shifted_character`, `keys_without_an_ascii_layout_character_are_named_by_position`, `alt_types_layout_text`, `command_and_control_type_nothing`, `control_keys_have_fixed_text`, `keypad_keys_keep_their_own_names`, `keys_outside_the_table_use_their_single_character`, `dead_keys_type_nothing`, `key_up_carries_the_keystroke`, `both_shift_keys_must_be_released`, `modifier_keys_combine`, `caps_lock_reports_its_state`, `a_key_without_shift_drops_a_lost_shift_release`, `a_handled_key_down_repeats_until_its_key_up`, `a_key_down_left_to_uikit_does_not_repeat_and_stops_the_repeat`, `the_last_handled_key_down_takes_over_the_repeat`, `modifier_and_caps_lock_keys_stop_the_repeat`, `stopping_the_repeat_ends_its_timer_generation`, `repeat_timing_is_the_wayland_default`, `key_up_reaches_uikit_only_when_its_key_down_did`.
- Command (`Ru-Test` trailer): `cargo test -p gpui_ios --features runtime_shaders --lib -- keyboard::tests`.
- Plan acceptance: simulator `press_key` reaches the app; a held key repeats on a device with a hardware keyboard.

Upstream: not upstream. Draft PR candidate.

## F8 gpui, gpui_linux: CursorStyle::None hides the cursor until another style is set

Adds `CursorStyle::None` back to GPUI (upstream removed it in #55664 when hiding on typing moved to `hide_cursor_until_mouse_moves`) with persistent semantics: the pointer stays hidden over the window, also while the mouse moves, until a later frame sets another style, like CSS `cursor: none`. Remote Ultra's viewer draws the remote cursor over the stream and needs the local one hidden; on X11, macOS and Windows it also hides it natively, Wayland has no other way. X11 sets the invisible cursor it already creates for `hide_cursor_until_mouse_moves` on the window; Wayland clears the pointer surface with `wl_pointer.set_cursor(None)` through one `show_cursor_style` helper that every place showing a style uses (set, pointer enter, hide-until-move restore, blocked window). `cursor_style_to_icon_names` and `to_shape` return `None` for it. gpui_macos registers a cursor without pixels for the view, gpui_windows's `load_cursor` returns no handle, so `WM_SETCURSOR` sets none, and gpui_web sets CSS `cursor: none`.

Files:
- `crates/gpui/src/platform.rs`
- `crates/gpui_linux/src/linux/platform.rs`
- `crates/gpui_linux/src/linux/wayland.rs`
- `crates/gpui_linux/src/linux/wayland/client.rs`
- `crates/gpui_linux/src/linux/x11/client.rs`
- `crates/gpui_macos/src/window.rs`
- `crates/gpui_web/src/platform.rs`
- `crates/gpui_windows/src/util.rs`

Tests:
- `gpui_linux` (Linux dev container): `linux::platform::tests::only_the_hidden_cursor_has_no_icon`, `linux::wayland::tests::only_the_hidden_cursor_has_no_shape`.
- `gpui_windows` (wine64): `util::tests::the_hidden_cursor_has_no_cursor_handle`.
- `gpui_macos` (macOS host): `window::tests::the_hidden_cursor_draws_nothing_and_is_not_the_arrow`.
- Commands (`Ru-Test` trailers): `linux -p gpui_linux --lib -- only_the_hidden_cursor_has_no_`, `x86_64-pc-windows-gnu -p gpui_windows --features test-support --lib -- util::tests::the_hidden_cursor_has_no_cursor_handle`, `host -p gpui_macos --features gpui_apple/runtime_shaders --lib -- window::tests::the_hidden_cursor_draws_nothing_and_is_not_the_arrow`.
- `gpui_web` builds only for `wasm32`, which none of the checks targets: check it with `cargo check -p gpui_web --target wasm32-unknown-unknown`.
- Plan acceptance (U3): the cursor stays hidden over the stream on a Linux box under X11 and Wayland.

Upstream: not upstream. Fork only.

## F10 gpui_ios: request 80-120 Hz from the display link on ProMotion displays

When the scene's screen reports `maximumFramesPerSecond() >= 120`, the `CADisplayLink` asks for `preferredFrameRateRange(80, 120, 120)`, so frames arrive at the full refresh rate and never less often than every 12.5 ms. iPhones also need `CADisableMinimumFrameDurationOnPhone` in the app's Info.plist.

Files:
- `crates/gpui_ios/Cargo.toml` (`objc2-quartz-core` feature `CAFrameRateRange`)
- `crates/gpui_ios/src/gpui_ios.rs`
- `crates/gpui_ios/src/frame_rate.rs` (new; compiled for iOS and for tests on any host)
- `crates/gpui_ios/src/ios/application.rs`

Tests (`gpui_ios::frame_rate::tests`, run on the macOS host): `promotion_screens_ask_for_80_to_120_hz`, `screens_below_120_hz_keep_the_system_rate`, `the_promotion_range_is_valid_and_bounds_the_frame_interval`.
- Command (`Ru-Test` trailer): `cargo test -p gpui_ios --features runtime_shaders --lib -- frame_rate::tests`.
- Plan acceptance: logged display link interval p50 <= 8.6 ms on a ProMotion device.

Upstream: not upstream. Fork only; it is not in the plan's list of draft PRs.
