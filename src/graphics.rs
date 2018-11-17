use gfx_hal::{
    command::{ClearColor, ClearValue},
    format::{Aspects, ChannelType, Format, Swizzle},
    image::{self, Layout, SubresourceRange, ViewKind},
    pass::{
        Attachment, AttachmentLoadOp, AttachmentOps, AttachmentStoreOp, SubpassDependency,
        SubpassDesc, SubpassRef,
    },
    pool::CommandPoolCreateFlags,
    pso::{PipelineStage, Rect, Viewport},
    Adapter, Backbuffer, Backend, CommandPool, Device, FrameSync, Instance, PhysicalDevice,
    PresentMode, QueueGroup, Submission, Surface, SwapImageIndex, Swapchain, SwapchainConfig,
};
use gfx_memory::{MemoryAllocator, SmartAllocator};
use imgui::{ImGui, Ui};
use imgui_gfx_hal;
use log::error;

struct SwapchainState<B: Backend> {
    swapchain: B::Swapchain,
    viewport: Viewport,
    framebuffers: Vec<B::Framebuffer>,
    frame_views: Vec<B::ImageView>,
}

pub struct Graphics<I: Instance> {
    surface: <I::Backend as Backend>::Surface,
    adapter: Adapter<I::Backend>,
    device: <I::Backend as Backend>::Device,
    queue_group: QueueGroup<I::Backend, gfx_hal::Graphics>,
    command_pool: CommandPool<I::Backend, gfx_hal::Graphics>,
    allocator: SmartAllocator<I::Backend>,
    swapchain_state: Option<SwapchainState<I::Backend>>,
    render_pass: <I::Backend as Backend>::RenderPass,
    frame_semaphore: <I::Backend as Backend>::Semaphore,
    frame_fence: <I::Backend as Backend>::Fence,
    imgui_renderer: imgui_gfx_hal::Renderer<I::Backend>,
    color_format: Format,
}

#[cfg(feature = "renderdoc")]
pub mod renderdoc {
    use log::{error, info};
    use renderdoc::{self, prelude::*, V112};

    pub type RenderDoc = Option<renderdoc::RenderDoc<V112>>;

    pub fn init() -> RenderDoc {
        match renderdoc::RenderDoc::new() {
            Ok(rd) => Some(rd),
            Err(err) => {
                error!("Renderdoc failed to init: {}", err);
                None
            }
        }
    }

    pub fn trigger_capture(rd: &mut RenderDoc, n_frames: u32) {
        if let Some(rd) = rd.as_mut() {
            info!("Triggering renderdoc capture");
            if n_frames == 1 {
                rd.trigger_capture();
            } else {
                rd.trigger_multi_frame_capture(n_frames);
            }
        }
    }
}

#[cfg(not(feature = "renderdoc"))]
pub mod renderdoc {
    type RenderDoc = ();

    pub fn init() -> RenderDoc {
        ()
    }

    pub fn trigger_capture(_: &mut RenderDoc) {}
}

impl<I: Instance> Graphics<I> {
    pub fn new(
        instance: I,
        surface: <I::Backend as Backend>::Surface,
        imgui: &mut ImGui,
    ) -> Graphics<I> {
        let mut adapters = instance.enumerate_adapters().into_iter();

        let (adapter, device, mut queue_group) = loop {
            let adapter = adapters.next().expect("No suitable adapter found");
            match adapter.open_with::<_, gfx_hal::Graphics>(1, |family| {
                surface.supports_queue_family(family)
            }) {
                Ok((device, queue_group)) => break (adapter, device, queue_group),
                Err(_) => (),
            }
        };
        let physical_device = &adapter.physical_device;

        let max_buffers = 16;
        let mut command_pool = device
            .create_command_pool_typed(&queue_group, CommandPoolCreateFlags::empty(), max_buffers)
            .unwrap();
        let mut allocator =
            SmartAllocator::new(physical_device.memory_properties(), 4096, 16, 512, 2048);

        // determine image capabilities and color format
        let (_, formats, _) = surface.compatibility(physical_device);
        let color_format = formats.map_or(Format::Rgba8Srgb, |formats| {
            formats
                .iter()
                .find(|format| format.base_format().1 == ChannelType::Srgb)
                .cloned()
                .unwrap_or(formats[0])
        });

        let render_pass = {
            let color_attachment = Attachment {
                format: Some(color_format),
                samples: 1,
                ops: AttachmentOps::new(AttachmentLoadOp::Clear, AttachmentStoreOp::Store),
                stencil_ops: AttachmentOps::DONT_CARE,
                layouts: Layout::Undefined..Layout::Present,
            };

            let subpass = SubpassDesc {
                colors: &[(0, Layout::ColorAttachmentOptimal)],
                depth_stencil: None,
                inputs: &[],
                resolves: &[],
                preserves: &[],
            };

            let dependency = SubpassDependency {
                passes: SubpassRef::External..SubpassRef::Pass(0),
                stages: PipelineStage::COLOR_ATTACHMENT_OUTPUT
                    ..PipelineStage::COLOR_ATTACHMENT_OUTPUT,
                accesses: image::Access::empty()
                    ..(image::Access::COLOR_ATTACHMENT_READ
                        | image::Access::COLOR_ATTACHMENT_WRITE),
            };

            device
                .create_render_pass(&[color_attachment], &[subpass], &[dependency])
                .unwrap()
        };

        let imgui_renderer = imgui_gfx_hal::Renderer::new(
            imgui,
            &device,
            physical_device,
            &render_pass,
            &mut command_pool,
            &mut queue_group,
        )
        .unwrap();

        let frame_semaphore = device.create_semaphore().unwrap();
        let frame_fence = device.create_fence(false).unwrap();

        Graphics {
            surface,
            adapter,
            device,
            queue_group,
            command_pool,
            allocator,
            swapchain_state: None,
            render_pass,
            frame_semaphore,
            frame_fence,
            imgui_renderer,
            color_format,
        }
    }

    pub fn resize(&mut self) {
        if let Some(swapchain_state) = self.swapchain_state.take() {
            self.device.wait_idle().unwrap();
            swapchain_state.destroy(&self.device);
        }
    }

    pub fn draw_frame(&mut self, ui: Ui) -> Result<(), ()> {
        self.device.reset_fence(&self.frame_fence).unwrap();
        self.command_pool.reset();

        if let None = self.swapchain_state {
            // Swapchain needs to be re-created.
            self.swapchain_state = Some(SwapchainState::new(
                &self.device,
                &self.adapter.physical_device,
                &mut self.surface,
                &self.render_pass,
                self.color_format,
            ));
        }
        let swapchain_state = self.swapchain_state.as_mut().unwrap();

        // Get swapchain index
        let frame_index: SwapImageIndex = swapchain_state
            .swapchain
            .acquire_image(!0, FrameSync::Semaphore(&mut self.frame_semaphore))
            .unwrap();

        let submit = {
            let mut cmd_buffer = self.command_pool.acquire_command_buffer(false);
            {
                let mut encoder = cmd_buffer.begin_render_pass_inline(
                    &self.render_pass,
                    &swapchain_state.framebuffers[frame_index as usize],
                    swapchain_state.viewport.rect,
                    &[ClearValue::Color(ClearColor::Float([0.1, 0.1, 0.1, 1.0]))],
                );
                self.imgui_renderer
                    .render(
                        ui,
                        &mut encoder,
                        &self.device,
                        &self.adapter.physical_device,
                    )
                    .unwrap();
            }
            cmd_buffer.finish()
        };

        let submission = Submission::new()
            .wait_on(&[(&self.frame_semaphore, PipelineStage::BOTTOM_OF_PIPE)])
            .submit(Some(submit));
        self.queue_group.queues[0].submit(submission, Some(&mut self.frame_fence));
        self.device.wait_for_fence(&self.frame_fence, !0).unwrap();

        if let Err(_) =
            swapchain_state
                .swapchain
                .present(&mut self.queue_group.queues[0], frame_index, &[])
        {
            error!("Error occurred while presenting swapchain.");
            return Err(());
        }

        Ok(())
    }

    pub fn destroy(self) {
        let Graphics {
            device,
            command_pool,
            frame_semaphore,
            frame_fence,
            render_pass,
            swapchain_state,
            mut allocator,
            imgui_renderer,
            ..
        } = self;

        if let Some(swapchain_state) = swapchain_state {
            swapchain_state.destroy(&device);
        }
        device.destroy_command_pool(command_pool.into_raw());
        device.destroy_fence(frame_fence);
        device.destroy_semaphore(frame_semaphore);
        device.destroy_render_pass(render_pass);
        imgui_renderer.destroy(&device);
        allocator.dispose(&device).unwrap();
    }
}

impl<B: Backend> SwapchainState<B> {
    fn new(
        device: &B::Device,
        physical_device: &B::PhysicalDevice,
        surface: &mut B::Surface,
        render_pass: &B::RenderPass,
        color_format: Format,
    ) -> SwapchainState<B> {
        let (caps, _, _) = surface.compatibility(physical_device);
        let swapchain_config =
            SwapchainConfig::from_caps(&caps, color_format).with_mode(PresentMode::Fifo);
        let (swapchain, backbuffer) = device
            .create_swapchain(surface, swapchain_config, None)
            .unwrap();
        let extent = caps.current_extent.unwrap();

        let (frame_views, framebuffers) = match backbuffer {
            Backbuffer::Images(images) => {
                let color_range = SubresourceRange {
                    aspects: Aspects::COLOR,
                    levels: 0..1,
                    layers: 0..1,
                };

                let image_views = images
                    .iter()
                    .map(|image| {
                        device
                            .create_image_view(
                                image,
                                ViewKind::D2,
                                color_format,
                                Swizzle::NO,
                                color_range.clone(),
                            )
                            .unwrap()
                    })
                    .collect::<Vec<_>>();

                let fbos = image_views
                    .iter()
                    .map(|image_view| {
                        device
                            .create_framebuffer(&render_pass, vec![image_view], extent.to_extent())
                            .unwrap()
                    })
                    .collect();

                (image_views, fbos)
            }
            Backbuffer::Framebuffer(fbo) => (vec![], vec![fbo]),
        };

        let viewport = Viewport {
            rect: Rect {
                x: 0,
                y: 0,
                w: extent.width as i16,
                h: extent.height as i16,
            },
            depth: 0.0..1.0,
        };

        SwapchainState {
            swapchain,
            viewport,
            framebuffers,
            frame_views,
        }
    }

    fn destroy(self, device: &B::Device) {
        device.destroy_swapchain(self.swapchain);
        for framebuffer in self.framebuffers {
            device.destroy_framebuffer(framebuffer);
        }
        for image_view in self.frame_views {
            device.destroy_image_view(image_view);
        }
    }
}
