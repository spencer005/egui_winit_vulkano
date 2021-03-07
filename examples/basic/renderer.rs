use crate::frame_system::{FrameSystem, Pass};
use egui_winit_vulkan::{EguiContext, EguiVulkanoRenderer};
use std::sync::Arc;
use vulkano::device::{Device, DeviceExtensions, Features, Queue};
use vulkano::image::{ImageUsage, SwapchainImage};
use vulkano::instance::{Instance, InstanceExtensions, PhysicalDevice};
use vulkano::swapchain;
use vulkano::swapchain::{
    AcquireError, ColorSpace, FullscreenExclusive, PresentMode, Surface, SurfaceTransform,
    Swapchain, SwapchainCreationError,
};
use vulkano::sync::{FlushError, GpuFuture};
use vulkano_win::VkSurfaceBuild;
use winit::event::Event;
use winit::event_loop::EventLoop;
use winit::window::{Window, WindowBuilder};

pub struct VulkanoWinitRenderer {
    instance: Arc<Instance>,
    device: Arc<Device>,
    surface: Arc<Surface<Window>>,
    queue: Arc<Queue>,
    swap_chain: Arc<Swapchain<Window>>,
    images: Vec<Arc<SwapchainImage<Window>>>,
    recreate_swapchain: bool,
    previous_frame_end: Option<Box<dyn GpuFuture>>,
    egui_context: EguiContext,
    // Deferred draw system for Egui
    egui_draw_system: EguiVulkanoRenderer,
    // Add deferred draw systems here, e.g. egui, or scene
}

impl VulkanoWinitRenderer {
    pub fn new(
        event_loop: &EventLoop<()>,
        width: u32,
        height: u32,
        present_mode: PresentMode,
    ) -> Self {
        // Add instance extensions based on needs
        let instance_extensions = InstanceExtensions {
            ..vulkano_win::required_extensions()
        };
        // Create instance
        let instance =
            Instance::new(None, &instance_extensions, None).expect("Failed to create instance");
        // Get most performant device (physical)
        let physical = PhysicalDevice::enumerate(&instance)
            .fold(None, |acc, val| {
                if acc.is_none() {
                    Some(val)
                } else {
                    if acc.unwrap().limits().max_compute_shared_memory_size()
                        >= val.limits().max_compute_shared_memory_size()
                    {
                        acc
                    } else {
                        Some(val)
                    }
                }
            })
            .expect("No physical device found");
        println!(
            "Using device: {} (type: {:?})",
            physical.name(),
            physical.ty()
        );
        // Create rendering surface along with window
        let surface = WindowBuilder::new()
            .with_inner_size(winit::dpi::LogicalSize::new(width, height))
            .build_vk_surface(&event_loop, instance.clone())
            .expect("Failed to create vulkan surface & window");
        // Create device
        let (device, queue) = Self::create_device(physical, surface.clone());
        // Create swap chain & frame(s) to which we'll render
        let (swap_chain, images) = Self::create_swap_chain(
            surface.clone(),
            physical,
            device.clone(),
            queue.clone(),
            present_mode,
        );
        let previous_frame_end = Some(sync::now(device.clone()).boxed());

        // Create frame system
        let frame_system = FrameSystem::new(queue.clone(), swap_chain.format());
        // Create subpass for EguiVulkanoRenderer
        let egui_draw_system =
            EguiVulkanoRenderer::new(queue.clone(), frame_system.deferred_subpass());
        // Create egui context
        let egui_context = EguiContext::new(
            surface.window().inner_size(),
            surface.window().scale_factor(),
        );
        Self {
            instance,
            device,
            surface,
            queue,
            swap_chain,
            images,
            previous_frame_end,
            recreate_swapchain: false,
            egui_context,
            egui_draw_system,
        }
    }

    /// Creates vulkan device with required queue families and required extensions
    /// We need khr_external_memory_fd for CUDA + Vulkan interoperability
    fn create_device(
        physical: PhysicalDevice,
        surface: Arc<Surface<Window>>,
    ) -> (Arc<Device>, Arc<Queue>) {
        let queue_family = physical
            .queue_families()
            .find(|&q| q.supports_graphics() && surface.is_supported(q).unwrap_or(false))
            .expect("couldn't find a graphical queue family");
        // Add device extensions based on needs
        let device_extensions = DeviceExtensions {
            ..DeviceExtensions::supported_by_device(physical)
        };
        // Add device features
        let features = Features {
            ..*physical.supported_features()
        };
        let (device, mut queues) = {
            Device::new(
                physical,
                &features,
                &device_extensions,
                [(queue_family, 0.5)].iter().cloned(),
            )
            .expect("failed to create device")
        };
        (device, queues.next().unwrap())
    }

    fn create_swap_chain(
        surface: Arc<Surface<Window>>,
        physical: PhysicalDevice,
        device: Arc<Device>,
        queue: Arc<Queue>,
        present_mode: PresentMode,
    ) -> (Arc<Swapchain<Window>>, Vec<Arc<SwapchainImage<Window>>>) {
        let (swap_chain, images) = {
            let caps = surface.capabilities(physical).unwrap();
            let alpha = caps.supported_composite_alpha.iter().next().unwrap();
            let format = caps.supported_formats[0].0;
            let dimensions: [u32; 2] = surface.window().inner_size().into();
            Swapchain::new(
                device.clone(),
                surface.clone(),
                caps.min_image_count,
                format,
                dimensions,
                1,
                ImageUsage::color_attachment(),
                &queue,
                SurfaceTransform::Identity,
                alpha,
                present_mode,
                FullscreenExclusive::Default,
                true,
                ColorSpace::SrgbNonLinear,
            )
            .unwrap()
        };
        (swap_chain, images)
    }

    /// Updates egui context with winit events, this must be called in event loop
    /// See main.rs
    pub fn egui_update<T>(&mut self, winit_event: &Event<T>) {
        self.egui_context.handle_event(winit_event)
    }

    pub fn device(&self) -> Arc<Device> {
        self.device.clone()
    }

    pub fn window(&self) -> &Window {
        self.surface.window()
    }

    pub fn resize(&mut self) {
        self.recreate_swapchain = true;
    }

    pub fn render(&mut self) {
        // Recreate swap chain if needed (when resizing of window occurs or swapchain is outdated)
        if self.recreate_swapchain {
            self.recreate_swapchain();
        }
        // Acquire next image in the swapchain
        let (image_num, suboptimal, acquire_future) =
            match swapchain::acquire_next_image(self.swap_chain.clone(), None) {
                Ok(r) => r,
                Err(AcquireError::OutOfDate) => {
                    self.recreate_swapchain = true;
                    return;
                }
                Err(e) => panic!("Failed to acquire next image: {:?}", e),
            };
        if suboptimal {
            self.recreate_swapchain = true;
        }
        // Acquire frame to which we'll render (by image_num)
        let future = self.previous_frame_end.take().unwrap().join(acquire_future);
        let mut frame = self.frame_system.frame(
            future,
            self.images[image_num].clone(),
            camera.world_to_screen(self.surface.window().scale_factor()),
        );

        // Draw each render pass
        let mut after_future = None;
        let mut winit_cursor_icon = None;
        while let Some(pass) = frame.next_pass() {
            match pass {
                Pass::Deferred(mut draw_pass) => {
                    // Add UI
                    // Update egui elapsed frame time
                    self.egui_context.begin_frame();
                    // ToDo: Ui content here
                    let (output, clipped_meshes) = self.egui_context.end_frame();
                    // Draw egui meshes
                    let cb = self.egui_draw_system.draw(
                        &mut self.egui_context,
                        clipped_meshes,
                        draw_pass.viewport_dimensions(),
                    );
                    draw_pass.execute(cb);
                }
                Pass::Finished(af) => {
                    after_future = Some(af);
                }
            }
        }
        // Convert winit cursor icon to egui
        self.window()
            .set_cursor_icon(EguiContext::egui_to_winit_cursor_icon(
                winit_cursor_icon.unwrap(),
            ));
        // Finish render
        self.finish(after_future, image_num);
    }

    /// Swapchain is recreated when resized
    fn recreate_swapchain(&mut self) {
        let dimensions: [u32; 2] = self.surface.window().inner_size().into();
        let (new_swapchain, new_images) = match self.swap_chain.recreate_with_dimensions(dimensions)
        {
            Ok(r) => r,
            Err(SwapchainCreationError::UnsupportedDimensions) => return,
            Err(e) => panic!("Failed to recreate swapchain: {:?}", e),
        };

        self.swap_chain = new_swapchain;
        self.images = new_images;
        self.recreate_swapchain = false;
    }

    /// Finishes render by presenting the swapchain
    fn finish(&mut self, after_future: Option<Box<dyn GpuFuture>>, image_num: usize) {
        let future = after_future
            .unwrap()
            .then_swapchain_present(self.queue.clone(), self.swap_chain.clone(), image_num)
            .then_signal_fence_and_flush();
        match future {
            Ok(future) => {
                // A hack to prevent OutOfMemory error on Nvidia :(
                // https://github.com/vulkano-rs/vulkano/issues/627
                match future.wait(None) {
                    Ok(x) => x,
                    Err(err) => println!("err: {:?}", err),
                }
                self.previous_frame_end = Some(future.boxed());
            }
            Err(FlushError::OutOfDate) => {
                self.recreate_swapchain = true;
                self.previous_frame_end = Some(sync::now(self.device.clone()).boxed());
            }
            Err(e) => {
                println!("Failed to flush future: {:?}", e);
                self.previous_frame_end = Some(sync::now(self.device.clone()).boxed());
            }
        }
    }
}