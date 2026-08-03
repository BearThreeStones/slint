// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use i_slint_core::api::{PhysicalSize as PhysicalWindowSize, Window};
use i_slint_core::graphics::RequestedGraphicsAPI;
use i_slint_core::partial_renderer::DirtyRegion;

use vulkano::device::physical::{PhysicalDevice, PhysicalDeviceType};
use vulkano::device::{
    Device, DeviceCreateInfo, DeviceExtensions, DeviceFeatures, Queue, QueueCreateInfo, QueueFlags,
};
use vulkano::image::view::ImageView;
use vulkano::image::{Image, ImageUsage};
use vulkano::instance::{Instance, InstanceCreateFlags, InstanceCreateInfo, InstanceExtensions};
use vulkano::swapchain::{Surface, Swapchain, SwapchainCreateInfo, SwapchainPresentInfo};
use vulkano::sync::GpuFuture;
use vulkano::{Handle, Validated, VulkanError, VulkanLibrary, VulkanObject, sync};

use crate::SkiaSharedContext;

pub struct SharedVulkanContext {
    instance: Arc<Instance>,
    // TODO: share also physical/logical device and queue, but their selection process is surface compatibility dependent.
}

impl super::SkiaSharedContextInner {
    fn shared_vulkan_context(
        &self,
    ) -> Result<&SharedVulkanContext, i_slint_core::platform::PlatformError> {
        if let Some(ctx) = self.vulkan_context.get() {
            return Ok(ctx);
        }
        self.vulkan_context.set(SharedVulkanContext::new()?).ok();
        Ok(self.vulkan_context.get().unwrap())
    }
}

impl SharedVulkanContext {
    fn new() -> Result<Self, i_slint_core::platform::PlatformError> {
        let library = VulkanLibrary::new()
            .map_err(|load_err| format!("Error loading vulkan library: {load_err}"))?;

        let required_extensions = InstanceExtensions {
            khr_surface: true,
            mvk_macos_surface: true,
            ext_metal_surface: true,
            khr_wayland_surface: true,
            khr_xlib_surface: true,
            khr_xcb_surface: true,
            khr_win32_surface: true,
            khr_get_surface_capabilities2: true,
            khr_get_physical_device_properties2: true,
            ..InstanceExtensions::empty()
        }
        .intersection(library.supported_extensions());

        let instance = Instance::new(
            library.clone(),
            InstanceCreateInfo {
                flags: InstanceCreateFlags::ENUMERATE_PORTABILITY,
                enabled_extensions: required_extensions,
                ..Default::default()
            },
        )
        .map_err(|instance_err| format!("Error creating Vulkan instance: {instance_err}"))?;
        Ok(Self { instance })
    }
}

/// Blunder: matches [`super::blunder_partial_rendering_enabled`].
fn blunder_partial_rendering_enabled() -> bool {
    match std::env::var("BLUNDER_SLINT_PARTIAL").as_deref() {
        Ok(value) => {
            let value = value.trim();
            !(value == "0" || value.eq_ignore_ascii_case("false"))
        }
        Err(_) => true,
    }
}

/// This surface renders into the given window using Vulkan.
pub struct VulkanSurface {
    gr_context: RefCell<skia_safe::gpu::DirectContext>,
    recreate_swapchain: Cell<bool>,
    device: Arc<Device>,
    previous_frame_end: RefCell<Option<Box<dyn GpuFuture>>>,
    queue: Arc<Queue>,
    swapchain: RefCell<Arc<Swapchain>>,
    swapchain_images: RefCell<Vec<Arc<Image>>>,
    swapchain_image_views: RefCell<Vec<Arc<ImageView>>>,
    /// Monotonic frame counter for swapchain buffer-age tracking.
    present_generation: Cell<u64>,
    /// Last frame generation each swapchain image was presented.
    image_last_presented: RefCell<Vec<u64>>,
}

impl VulkanSurface {
    /// Creates a Skia Vulkan rendering surface from the given Vukano device, queue family index, surface,
    /// and size.
    pub fn from_surface(
        physical_device: Arc<PhysicalDevice>,
        queue_family_index: u32,
        surface: Arc<Surface>,
        size: PhysicalWindowSize,
    ) -> Result<Self, i_slint_core::platform::PlatformError> {
        /*
        eprintln!(
            "Vulkan device: {} (type: {:?})",
            physical_device.properties().device_name,
            physical_device.properties().device_type,
        );*/

        let (device, mut queues) = Device::new(
            physical_device.clone(),
            DeviceCreateInfo {
                enabled_extensions: DeviceExtensions {
                    khr_swapchain: true,
                    ..DeviceExtensions::empty()
                },
                queue_create_infos: vec![QueueCreateInfo {
                    queue_family_index,
                    ..Default::default()
                }],
                ..Default::default()
            },
        )
        .map_err(|dev_err| format!("Failed to create suitable logical Vulkan device: {dev_err}"))?;
        let queue = queues.next().ok_or_else(|| "Not Vulkan device queue found".to_string())?;

        Self::from_device_queue_surface(device, queue, surface, size)
    }

    /// Blunder: builds the Skia Vulkan swapchain + context on an already-created
    /// logical `device`/`queue`. Shared by the self-owned (`from_surface`) and
    /// the engine-owned (`from_shared_handles`) device paths.
    fn from_device_queue_surface(
        device: Arc<Device>,
        queue: Arc<Queue>,
        surface: Arc<Surface>,
        size: PhysicalWindowSize,
    ) -> Result<Self, i_slint_core::platform::PlatformError> {
        let (swapchain, swapchain_images) = {
            let surface_capabilities = device
                .physical_device()
                .surface_capabilities(&surface, Default::default())
                .map_err(|vke| format!("Error matching Vulkan surface capabilities: {vke}"))?;
            let image_format = vulkano::format::Format::B8G8R8A8_UNORM;

            Swapchain::new(
                device.clone(),
                surface.clone(),
                SwapchainCreateInfo {
                    min_image_count: surface_capabilities.min_image_count,
                    image_format,
                    image_extent: [size.width, size.height],
                    image_usage: ImageUsage::COLOR_ATTACHMENT,
                    composite_alpha: surface_capabilities
                        .supported_composite_alpha
                        .into_iter()
                        .next()
                        .ok_or_else(|| {
                            "fatal: Vulkan surface capabilities missing composite alpha descriptor"
                                .to_string()
                        })?,
                    ..Default::default()
                },
            )
            .map_err(|vke| format!("Error creating Vulkan swapchain: {vke}"))?
        };

        let mut swapchain_image_views = Vec::with_capacity(swapchain_images.len());

        for image in &swapchain_images {
            swapchain_image_views.push(ImageView::new_default(image.clone()).map_err(|vke| {
                format!("fatal: Error creating image view for swap chain image: {vke}")
            })?);
        }

        let physical_device = device.physical_device().clone();
        let instance = physical_device.instance().clone();
        let library = instance.library().clone();

        let get_proc = |of| unsafe {
            let result = match of {
                skia_safe::gpu::vk::GetProcOf::Instance(instance_handle, name) => {
                    library.get_instance_proc_addr(
                        ash::vk::Instance::from_raw(instance_handle as _),
                        name,
                    )
                }
                skia_safe::gpu::vk::GetProcOf::Device(device_handle, name) => {
                    (instance.fns().v1_0.get_device_proc_addr)(
                        ash::vk::Device::from_raw(device_handle as _),
                        name,
                    )
                }
            };

            match result {
                Some(f) => f as _,
                None => {
                    //println!("resolve of {} failed", of.name().to_str().unwrap());
                    core::ptr::null()
                }
            }
        };

        // NOTE: Skia's `fGraphicsQueueIndex` is the queue *family* index, not the
        // index within the family. vulkano's `Queue::queue_index()` is the latter
        // (== 0 for the engine's single queue), so we must pass
        // `queue_family_index()` here. Using queue_index() only happens to work
        // when the graphics family is family 0.
        let backend_context = unsafe {
            skia_safe::gpu::vk::BackendContext::new(
                instance.handle().as_raw() as _,
                physical_device.handle().as_raw() as _,
                device.handle().as_raw() as _,
                (queue.handle().as_raw() as _, queue.queue_family_index() as _),
                &get_proc,
            )
        };

        // NOTE: make_vulkan() fails when the Vulkan validation layer is enabled
        // on this (externally-created) instance/device. The embedding app should
        // disable validation to use the shared device (the caller falls back to a
        // self-owned device otherwise).
        let gr_context = skia_safe::gpu::direct_contexts::make_vulkan(&backend_context, None)
            .ok_or_else(|| {
                "Error creating Skia Vulkan context on the shared device \
                 (is the Vulkan validation layer enabled?)"
                    .to_string()
            })?;

        let previous_frame_end = RefCell::new(Some(sync::now(device.clone()).boxed()));

        let image_count = swapchain_images.len();
        Ok(Self {
            gr_context: RefCell::new(gr_context),
            recreate_swapchain: Cell::new(false),
            device,
            previous_frame_end,
            queue,
            swapchain: RefCell::new(swapchain),
            swapchain_images: RefCell::new(swapchain_images),
            swapchain_image_views: RefCell::new(swapchain_image_views),
            present_generation: Cell::new(0),
            image_last_presented: RefCell::new(vec![0; image_count]),
        })
    }

    fn reset_swapchain_age_tracking(&self, image_count: usize) {
        self.present_generation.set(0);
        *self.image_last_presented.borrow_mut() = vec![0; image_count];
    }

    fn compute_back_buffer_age(&self, image_index: u32, frame_generation: u64) -> u8 {
        let last_presented = self.image_last_presented.borrow();
        let idx = image_index as usize;
        if idx >= last_presented.len() {
            return 0;
        }
        let last_gen = last_presented[idx];
        if last_gen == 0 {
            return 0;
        }
        let age = frame_generation.saturating_sub(last_gen);
        if age == 0 { 0 } else { age.min(3) as u8 }
    }

    /// Blunder: wraps the engine's existing Vulkan handles (instance, physical
    /// device, logical device, graphics-queue family) into vulkano objects via
    /// `from_handle`, then builds the Skia Vulkan swapchain surface on that
    /// shared device. The engine retains ownership of the Vulkan objects, so
    /// this surface (and the owning `SkiaRenderer`) must be dropped before the
    /// engine destroys the device. The features/extensions declared here must
    /// match what the engine created the device with (see `VulkanContext`).
    pub fn from_shared_handles(
        instance_handle: u64,
        physical_device_handle: u64,
        device_handle: u64,
        queue_family_index: u32,
        window_handle: raw_window_handle::WindowHandle<'_>,
        display_handle: raw_window_handle::DisplayHandle<'_>,
        size: PhysicalWindowSize,
    ) -> Result<Self, i_slint_core::platform::PlatformError> {
        let library = vulkano::VulkanLibrary::new()
            .map_err(|load_err| format!("Error loading vulkan library: {load_err}"))?;

        // Adopt the engine's VkInstance. The surface extensions must be declared
        // so vulkano permits creating a window surface from this instance.
        let instance = unsafe {
            Instance::from_handle(
                library,
                ash::vk::Instance::from_raw(instance_handle),
                InstanceCreateInfo {
                    enabled_extensions: InstanceExtensions {
                        khr_surface: true,
                        khr_win32_surface: true,
                        khr_get_physical_device_properties2: true,
                        khr_get_surface_capabilities2: true,
                        ..InstanceExtensions::empty()
                    },
                    ..Default::default()
                },
            )
        };

        let physical_device = unsafe {
            PhysicalDevice::from_handle(
                instance.clone(),
                ash::vk::PhysicalDevice::from_raw(physical_device_handle),
            )
        }
        .map_err(|vke| format!("Error adopting shared Vulkan physical device: {vke}"))?;

        // Adopt the engine's VkDevice and retrieve a handle to the shared queue.
        let (device, mut queues) = unsafe {
            Device::from_handle(
                physical_device,
                ash::vk::Device::from_raw(device_handle),
                DeviceCreateInfo {
                    queue_create_infos: vec![QueueCreateInfo {
                        queue_family_index,
                        ..Default::default()
                    }],
                    enabled_extensions: DeviceExtensions {
                        khr_swapchain: true,
                        ..DeviceExtensions::empty()
                    },
                    enabled_features: DeviceFeatures {
                        shader_draw_parameters: true,
                        sampler_anisotropy: true,
                        geometry_shader: true,
                        ..DeviceFeatures::empty()
                    },
                    ..Default::default()
                },
            )
        };
        let queue =
            queues.next().ok_or_else(|| "No queue from shared Vulkan device".to_string())?;

        // The embedding engine owns the VkInstance and VkDevice. vulkano's
        // `from_handle` wrappers would otherwise call vkDestroyInstance /
        // vkDestroyDevice when dropped, double-freeing the engine's objects (and
        // tearing down the device mid-init on any failure). Leak one extra
        // reference to each so vulkano never destroys them; the engine remains
        // the sole owner and destroys them in VulkanContext::shutdown. The
        // surface and swapchain are vulkano-owned and still drop normally.
        std::mem::forget(instance.clone());
        std::mem::forget(device.clone());

        let surface = create_surface(&instance, window_handle, display_handle)
            .map_err(|surface_err| format!("Error creating Vulkan surface: {surface_err}"))?;

        Self::from_device_queue_surface(device, queue, surface, size)
    }

    /// Returns a clone of the shared swapchain.
    pub fn swapchain(&self) -> Arc<Swapchain> {
        self.swapchain.borrow().clone()
    }
}

impl super::Surface for VulkanSurface {
    fn new(
        shared_context: &SkiaSharedContext,
        window_handle: Arc<dyn raw_window_handle::HasWindowHandle + Send + Sync>,
        display_handle: Arc<dyn raw_window_handle::HasDisplayHandle + Send + Sync>,
        size: PhysicalWindowSize,
        requested_graphics_api: Option<RequestedGraphicsAPI>,
    ) -> Result<Self, i_slint_core::platform::PlatformError> {
        if requested_graphics_api.is_some_and(|api| !matches!(api, RequestedGraphicsAPI::Vulkan)) {
            return Err("Requested non-Vulkan rendering with Vulkan renderer".into());
        }

        let instance = shared_context.0.shared_vulkan_context()?.instance.clone();

        let window_handle = window_handle
            .window_handle()
            .map_err(|e| format!("error obtaining window handle for skia vulkan renderer: {e}"))?;
        let display_handle = display_handle
            .display_handle()
            .map_err(|e| format!("error obtaining display handle for skia vulkan renderer: {e}"))?;

        let surface = create_surface(&instance, window_handle, display_handle)
            .map_err(|surface_err| format!("Error creating Vulkan surface: {surface_err}"))?;

        let device_extensions =
            DeviceExtensions { khr_swapchain: true, ..DeviceExtensions::empty() };
        let (physical_device, queue_family_index) = instance
            .enumerate_physical_devices()
            .map_err(|vke| format!("Error enumerating physical Vulkan devices: {vke}"))?
            .filter(|p| p.supported_extensions().contains(&device_extensions))
            .filter_map(|p| {
                p.queue_family_properties()
                    .iter()
                    .enumerate()
                    .position(|(i, q)| {
                        q.queue_flags.intersects(QueueFlags::GRAPHICS)
                            && p.surface_support(i as u32, &surface).unwrap_or(false)
                    })
                    .map(|i| (p, i as u32))
            })
            .min_by_key(|(p, _)| match p.properties().device_type {
                PhysicalDeviceType::DiscreteGpu => 0,
                PhysicalDeviceType::IntegratedGpu => 1,
                PhysicalDeviceType::VirtualGpu => 2,
                PhysicalDeviceType::Cpu => 3,
                PhysicalDeviceType::Other => 4,
                _ => 5,
            })
            .ok_or_else(|| "Vulkan: Failed to find suitable physical device".to_string())?;

        Self::from_surface(physical_device, queue_family_index, surface, size)
    }

    fn name(&self) -> &'static str {
        "vulkan"
    }

    fn resize_event(
        &self,
        _size: PhysicalWindowSize,
    ) -> Result<(), i_slint_core::platform::PlatformError> {
        self.recreate_swapchain.set(true);
        Ok(())
    }

    fn render(
        &self,
        _window: &Window,
        size: PhysicalWindowSize,
        callback: &dyn Fn(
            &skia_safe::Canvas,
            Option<&mut skia_safe::gpu::DirectContext>,
            u8,
        ) -> Option<DirtyRegion>,
        pre_present_callback: &RefCell<Option<Box<dyn FnMut()>>>,
    ) -> Result<(), i_slint_core::platform::PlatformError> {
        let gr_context = &mut self.gr_context.borrow_mut();

        let device = self.device.clone();

        self.previous_frame_end.borrow_mut().as_mut().unwrap().cleanup_finished();

        if self.recreate_swapchain.take() {
            let mut swapchain = self.swapchain.borrow_mut();
            let (new_swapchain, new_images) = swapchain
                .recreate(SwapchainCreateInfo {
                    image_extent: [size.width, size.height],
                    ..swapchain.create_info()
                })
                .map_err(|vke| format!("Error re-creating Vulkan swap chain: {vke}"))?;

            *swapchain = new_swapchain;

            let mut new_swapchain_image_views = Vec::with_capacity(new_images.len());

            for image in &new_images {
                new_swapchain_image_views.push(ImageView::new_default(image.clone()).map_err(
                    |vke| format!("fatal: Error creating image view for swap chain image: {vke}"),
                )?);
            }

            let image_count = new_images.len();
            *self.swapchain_images.borrow_mut() = new_images;
            *self.swapchain_image_views.borrow_mut() = new_swapchain_image_views;
            self.reset_swapchain_age_tracking(image_count);
        }

        let swapchain = self.swapchain.borrow().clone();

        #[cfg_attr(slint_nightly_test, allow(non_exhaustive_omitted_patterns))]
        let (image_index, suboptimal, acquire_future) =
            match vulkano::swapchain::acquire_next_image(swapchain.clone(), None)
                .map_err(Validated::unwrap)
            {
                Ok(r) => r,
                Err(VulkanError::OutOfDate) => {
                    self.recreate_swapchain.set(true);
                    return Ok(()); // Try again next frame
                }
                Err(e) => return Err(format!("Vulkan: failed to acquire next image: {e}").into()),
            };

        if suboptimal {
            self.recreate_swapchain.set(true);
        }

        let width = swapchain.image_extent()[0];
        let width: i32 = width
            .try_into()
            .map_err(|_| format!("internal error: invalid swapchain image width {width}"))?;
        let height = swapchain.image_extent()[1];
        let height: i32 = height
            .try_into()
            .map_err(|_| format!("internal error: invalid swapchain image height {height}"))?;

        let image_view = self.swapchain_image_views.borrow()[image_index as usize].clone();
        let image_object = image_view.image();

        let format = image_view.format();

        debug_assert_eq!(format, vulkano::format::Format::B8G8R8A8_UNORM);
        let (vk_format, color_type) =
            (skia_safe::gpu::vk::Format::B8G8R8A8_UNORM, skia_safe::ColorType::BGRA8888);

        let alloc = skia_safe::gpu::vk::Alloc::default();
        let image_info = &unsafe {
            skia_safe::gpu::vk::ImageInfo::new(
                image_object.handle().as_raw() as _,
                alloc,
                skia_safe::gpu::vk::ImageTiling::OPTIMAL,
                skia_safe::gpu::vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk_format,
                1,
                None,
                None,
                None,
                None,
            )
        };

        let render_target =
            &skia_safe::gpu::backend_render_targets::make_vk((width, height), image_info);

        let mut skia_surface = skia_safe::gpu::surfaces::wrap_backend_render_target(
            gr_context,
            render_target,
            skia_safe::gpu::SurfaceOrigin::TopLeft,
            color_type,
            None,
            None,
        )
        .ok_or_else(|| "Error creating Skia Vulkan surface".to_string())?;

        let frame_generation = self.present_generation.get().saturating_add(1);
        let back_buffer_age = self.compute_back_buffer_age(image_index, frame_generation);

        callback(skia_surface.canvas(), Some(gr_context), back_buffer_age);

        drop(skia_surface);

        gr_context.submit(None);

        if let Some(pre_present_callback) = pre_present_callback.borrow_mut().as_mut() {
            pre_present_callback();
        }

        let future = self
            .previous_frame_end
            .borrow_mut()
            .take()
            .unwrap()
            .join(acquire_future)
            .then_swapchain_present(
                self.queue.clone(),
                SwapchainPresentInfo::swapchain_image_index(swapchain.clone(), image_index),
            )
            .then_signal_fence_and_flush();

        #[cfg_attr(slint_nightly_test, allow(non_exhaustive_omitted_patterns))]
        match future.map_err(Validated::unwrap) {
            Ok(future) => {
                {
                    let mut last_presented = self.image_last_presented.borrow_mut();
                    if (image_index as usize) < last_presented.len() {
                        last_presented[image_index as usize] = frame_generation;
                    }
                }
                self.present_generation.set(frame_generation);
                *self.previous_frame_end.borrow_mut() = Some(future.boxed());
            }
            Err(VulkanError::OutOfDate) => {
                self.recreate_swapchain.set(true);
                *self.previous_frame_end.borrow_mut() = Some(sync::now(device.clone()).boxed());
            }
            Err(e) => {
                *self.previous_frame_end.borrow_mut() = Some(sync::now(device.clone()).boxed());
                return Err(format!("Skia Vulkan renderer: failed to flush future: {e}").into());
            }
        }

        Ok(())
    }

    fn bits_per_pixel(&self) -> Result<u8, i_slint_core::platform::PlatformError> {
        #[cfg_attr(slint_nightly_test, allow(non_exhaustive_omitted_patterns))]
        Ok(match self.swapchain.borrow().image_format() {
            vulkano::format::Format::B8G8R8A8_UNORM => 32,
            fmt => {
                return Err(format!(
                    "Skia Vulkan Renderer: Unsupported swapchain image format found {fmt:?}"
                )
                .into());
            }
        })
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn use_partial_rendering(&self) -> bool {
        blunder_partial_rendering_enabled()
    }

    /// Blunder: wraps a borrowed engine `VkImage` (shared-device path) as a Skia
    /// image so the 3D viewport composites zero-copy (no CPU readback). The image
    /// must live on this surface's device and be in the declared layout.
    fn import_vulkan_texture(
        &self,
        canvas: &skia_safe::Canvas,
        texture: &i_slint_core::graphics::BorrowedVulkanTexture,
    ) -> Option<skia_safe::Image> {
        // The engine off-screen color image is an 8-bit UNORM target sampled in
        // SHADER_READ_ONLY layout. Map the known formats explicitly so we don't
        // depend on the bindgen representation of VkFormat.
        let (vk_format, color_type) = match texture.format {
            // VK_FORMAT_R8G8B8A8_UNORM
            37 => (
                skia_safe::gpu::vk::Format::R8G8B8A8_UNORM,
                skia_safe::ColorType::RGBA8888,
            ),
            // VK_FORMAT_B8G8R8A8_UNORM
            44 => (
                skia_safe::gpu::vk::Format::B8G8R8A8_UNORM,
                skia_safe::ColorType::BGRA8888,
            ),
            _ => return None,
        };

        let alloc = skia_safe::gpu::vk::Alloc::default();
        let image_info = unsafe {
            skia_safe::gpu::vk::ImageInfo::new(
                texture.image as _,
                alloc,
                skia_safe::gpu::vk::ImageTiling::OPTIMAL,
                skia_safe::gpu::vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk_format,
                1,
                None,
                None,
                None,
                None,
            )
        };

        let backend_texture = unsafe {
            skia_safe::gpu::backend_textures::make_vk(
                (texture.size.width as i32, texture.size.height as i32),
                &image_info,
                "Blunder viewport image",
            )
        };

        let origin = match texture.origin {
            i_slint_core::graphics::BorrowedOpenGLTextureOrigin::BottomLeft => {
                skia_safe::gpu::SurfaceOrigin::BottomLeft
            }
            _ => skia_safe::gpu::SurfaceOrigin::TopLeft,
        };

        skia_safe::image::Image::from_texture(
            canvas.recording_context().as_mut().unwrap(),
            &backend_texture,
            origin,
            color_type,
            skia_safe::AlphaType::Opaque,
            None,
        )
    }
}

// FIXME(madsmtm): Why are we doing this instead of using `Surface::from_window`?
fn create_surface(
    instance: &Arc<Instance>,
    window_handle: raw_window_handle::WindowHandle<'_>,
    display_handle: raw_window_handle::DisplayHandle<'_>,
) -> Result<Arc<Surface>, vulkano::Validated<vulkano::VulkanError>> {
    #[cfg_attr(slint_nightly_test, allow(non_exhaustive_omitted_patterns))]
    match (window_handle.as_raw(), display_handle.as_raw()) {
        #[cfg(target_vendor = "apple")]
        (raw_window_handle::RawWindowHandle::AppKit(handle), _) => unsafe {
            let layer = raw_window_metal::Layer::from_ns_view(handle.ns_view);
            Surface::from_metal(instance.clone(), layer.as_ptr().as_ptr(), None)
        },
        #[cfg(target_vendor = "apple")]
        (raw_window_handle::RawWindowHandle::UiKit(handle), _) => unsafe {
            let layer = raw_window_metal::Layer::from_ui_view(handle.ui_view);
            Surface::from_metal(instance.clone(), layer.as_ptr().as_ptr(), None)
        },
        (
            raw_window_handle::RawWindowHandle::Xlib(raw_window_handle::XlibWindowHandle {
                window,
                ..
            }),
            raw_window_handle::RawDisplayHandle::Xlib(display),
        ) => unsafe {
            Surface::from_xlib(instance.clone(), display.display.unwrap().as_ptr(), window, None)
        },
        (
            raw_window_handle::RawWindowHandle::Xcb(raw_window_handle::XcbWindowHandle {
                window,
                ..
            }),
            raw_window_handle::RawDisplayHandle::Xcb(raw_window_handle::XcbDisplayHandle {
                connection,
                ..
            }),
        ) => unsafe {
            Surface::from_xcb(instance.clone(), connection.unwrap().as_ptr(), window.get(), None)
        },
        (
            raw_window_handle::RawWindowHandle::Wayland(raw_window_handle::WaylandWindowHandle {
                surface,
                ..
            }),
            raw_window_handle::RawDisplayHandle::Wayland(raw_window_handle::WaylandDisplayHandle {
                display,
                ..
            }),
        ) => unsafe {
            Surface::from_wayland(instance.clone(), display.as_ptr(), surface.as_ptr(), None)
        },
        (
            raw_window_handle::RawWindowHandle::Win32(raw_window_handle::Win32WindowHandle {
                hwnd,
                hinstance,
                ..
            }),
            _,
        ) => unsafe {
            let hinst = hinstance.map(|h| h.get()).unwrap_or(0);
            Surface::from_win32(instance.clone(), hinst, hwnd.get(), None)
        },
        _ => unimplemented!(),
    }
}
