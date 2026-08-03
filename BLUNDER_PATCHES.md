# Blunder Engine patches (`blunder/v1.16.1`)

Branch based on upstream tag **v1.16.1**. Maintained for the
[Blunder Engine](https://github.com/BearThreeStones/Blunder-Engine) C++ custom
platform integration (SDL3 `HWND`, headless engine Vulkan, Slint UI composite).

## Commits (newest first)

| Area | Summary |
|------|---------|
| CMake `SlintMacro.cmake` | When `SLINT_FEATURE_LIVE_PREVIEW` is on, invoke `slint-compiler` with `SLINT_LIVE_PREVIEW=1` and stamp codegen mode so flipping the feature regenerates `.slint` stubs (Blunder editor UI hot-reload). |
| `cpp_live_preview.rs` | Live-preview `on_*` wrappers capture the functor with `std::forward` and `mutable` so non-const call operators (common in C++ UI bindings) satisfy `set_callback`. |
| C++ `slint_skia_renderer_new` | Windows now defaults to **Vulkan** composition (`SkiaRenderer::default_vulkan()`); set env `BLUNDER_SLINT_RENDERER=d3d12` to fall back to Direct3D. Enables sharing the engine's Vulkan device for a zero-copy 3D viewport. |
| C++ `slint_skia_renderer_new_vulkan_shared` (new FFI) + `SkiaRenderer::new_vulkan_shared` | Build a Skia Vulkan renderer on a **caller-owned** `VkInstance`/`VkPhysicalDevice`/`VkDevice` + graphics queue family (the engine's headless device). |
| Skia `VulkanSurface::from_shared_handles` | Adopt the engine's raw Vulkan handles via vulkano `from_handle`; `mem::forget` the instance/device wrappers so vulkano never destroys the engine-owned objects. Refactors `from_surface` to share `from_device_queue_surface`. |
| Skia `Surface::import_vulkan_texture` + `VulkanSurface` impl | Wrap a borrowed engine `VkImage` (`vk::ImageInfo` + `backend_textures::make_vk` + `Image::from_texture`) so the 3D viewport composites zero-copy. Fixes `fGraphicsQueueIndex` to use the queue **family** index. |
| core `graphics::BorrowedVulkanTexture` + `ImageInner::BorrowedVulkanTexture` | New image variant carrying a borrowed `VkImage` (handle/format/layout/size/origin); dispatched in `skia/cached_image.rs`. |
| C++ `Image::create_from_borrowed_vulkan_texture` | C++ API to build a `slint::Image` from a borrowed `VkImage` (mirrors `create_from_borrowed_gl_2d_rgba_texture`). |
| C++ `slint_skia_renderer_resize` | Exposes `SkiaRenderer::resize()` so Blunder can resize the swap chain on maximize without destroying/recreating the renderer. |
| Skia Vulkan partial composite | `VulkanSurface::use_partial_rendering()` + swapchain buffer-age tracking; C++ `SkiaRenderer::mark_dirty_region` / `force_full_refresh`. Enabled by default; set `BLUNDER_SLINT_PARTIAL=0` to disable. Debug with `SLINT_SKIA_PARTIAL_RENDERING=log`. |
| C++ `slint_new_raw_window_handle_win32` | Forward `hinstance` into `Win32WindowHandle` (upstream ignored the parameter). |
| Skia `VulkanSurface` | Use `hinstance` when present, else `0` for `Surface::from_win32` (no panic on `None`). |

## Shared-device zero-copy viewport (Blunder)

The engine renders the 3D viewport into an off-screen `VkImage` and the Slint
Skia renderer composites the editor UI. To avoid a CPU readback per frame, the
renderer adopts the **engine's** Vulkan device (`new_vulkan_shared`) and samples
the off-screen image directly via `BorrowedVulkanTexture` /
`import_vulkan_texture`. The engine selects the path at runtime
(`SlintSystem::viewportUsesSharedDevice()` ↔ `SkiaRenderer::uses_shared_vulkan()`).

**Validation-layer limitation:** Skia's `make_vulkan()` fails to create a
context on an externally-created device while the Vulkan validation layer is
loaded, so `new_vulkan_shared` returns null and the C++ `SkiaRenderer` falls
back to a self-owned device (CPU readback path). Release builds have validation
off and get the shared device automatically; in debug set
`BLUNDER_VK_VALIDATION=0` to exercise the zero-copy path.

## Upstreaming

The **hinstance** fixes are suitable for an upstream PR to [slint-ui/slint](https://github.com/slint-ui/slint).
The **D3D12 default in `slint_skia_renderer_new`** is Blunder-specific unless Slint adds a
configurable C++ backend feature.

## Upgrade procedure

```bash
cd engine/3rdparty/slint
git fetch origin
git fetch https://github.com/slint-ui/slint.git tag v1.17.0  # example
git checkout -B blunder/v1.17.0 v1.17.0
# cherry-pick or replay patches from blunder/v1.16.1
cmake --build <blunder-build-dir> --target slint_cpp
```

After moving the submodule commit, update the parent repo:

```bash
cd <Blunder-Engine-root>
git add engine/3rdparty/slint
```
