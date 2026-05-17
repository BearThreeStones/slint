# Blunder Engine patches (`blunder/v1.16.1`)

Branch based on upstream tag **v1.16.1**. Maintained for the
[Blunder Engine](https://github.com/BearThreeStones/Blunder-Engine) C++ custom
platform integration (SDL3 `HWND`, headless engine Vulkan, Slint UI composite).

## Commits (newest first)

| Area | Summary |
|------|---------|
| C++ `slint_skia_renderer_new` | Windows uses `SkiaRenderer::default_direct3d()` + `set_window_handle()` instead of `SkiaRenderer::new()` (Vulkan WSI). Avoids a second Vulkan swapchain on the SDL window. |
| C++ `slint_new_raw_window_handle_win32` | Forward `hinstance` into `Win32WindowHandle` (upstream ignored the parameter). |
| Skia `VulkanSurface` | Use `hinstance` when present, else `0` for `Surface::from_win32` (no panic on `None`). |

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
