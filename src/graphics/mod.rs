use arrayvec::ArrayVec;
use gfx_hal::{
    buffer,
    command::{ClearColor, ClearValue, Primary, RenderPassInlineEncoder},
    format::{Aspects, ChannelType, Format, Swizzle},
    image::{self, Layout, SubresourceRange, ViewKind},
    memory::{Barrier, Dependencies, Properties, Requirements},
    pass::{
        Attachment,
        AttachmentLoadOp,
        AttachmentOps,
        AttachmentStoreOp,
        SubpassDependency,
        SubpassDesc,
        SubpassRef,
    },
    pool::CommandPoolCreateFlags,
    pso::{DescriptorRangeDesc, DescriptorType, PipelineStage, Rect, Viewport},
    Adapter,
    Backbuffer,
    Backend,
    CommandPool,
    CommandQueue,
    Device,
    FrameSync,
    Instance,
    MemoryType,
    MemoryTypeId,
    PhysicalDevice,
    PresentMode,
    QueueFamily,
    QueueGroup,
    QueueType,
    Submission,
    Surface,
    SwapImageIndex,
    Swapchain,
    SwapchainConfig,
};
use imgui::{ImGui, Ui};
use imgui_gfx_hal;
use log::{error, info};
use std::mem;
use take_mut;

pub mod circles;

pub use self::circles::{Circle, CircleRenderer};

/// The maximum number of frames in flight.
pub const MAX_FRAMES: usize = 2;

#[repr(C, packed)]
struct GlobalUbo {
    scale: [f32; 2],
}

pub const GLOBAL_UBO_SIZE: u64 = mem::size_of::<GlobalUbo>() as u64;

struct SwapchainState<B: Backend> {
    swapchain: B::Swapchain,
    viewport: Viewport,
    framebuffers: Vec<B::Framebuffer>,
    frame_views: Vec<B::ImageView>,
}

/// This exists because nvidia GPUs have a separate queue family that
/// only supports transfer, but is faster than the generic graphics
/// queue family at transfering. Other GPUs don't appear have this, so
/// both need to be supported.
pub enum QueueGroups<B: Backend> {
    // TODO: there needs to be more stuff to handle gpus with separate
    // graphics and present queues. I'm not sure how often that
    // happens though
    Single(QueueGroup<B, gfx_hal::Graphics>),
    Separate {
        graphics: QueueGroup<B, gfx_hal::Graphics>,
        transfer: QueueGroup<B, gfx_hal::Transfer>,
    },
}

pub struct Graphics<B: Backend> {
    surface: B::Surface,
    adapter: Adapter<B>,
    device: B::Device,
    memory_types: Vec<MemoryType>,
    queue_groups: QueueGroups<B>,
    transfer_command_pool: CommandPool<B, gfx_hal::Transfer>,
    frame_command_pools:
        ArrayVec<[CommandPool<B, gfx_hal::Graphics>; MAX_FRAMES]>,
    swapchain_state: SwapchainState<B>,
    render_pass: B::RenderPass,
    global_ubo: B::Buffer,
    global_ubo_memory: B::Memory,
    descriptor_pool: B::DescriptorPool,
    image_available_semaphores: ArrayVec<[B::Semaphore; MAX_FRAMES]>,
    frame_finished_semaphores: ArrayVec<[B::Semaphore; MAX_FRAMES]>,
    global_ubo_update_semaphore: B::Semaphore,
    frame_fences: ArrayVec<[B::Fence; MAX_FRAMES]>,
    transfer_fence: B::Fence,
    imgui_renderer: imgui_gfx_hal::Renderer<B>,
    color_format: Format,
    present_mode: PresentMode,
    current_frame: usize,
    swapchain_update: bool,
    viewport_update: bool,
    first_frame: bool,
}

pub struct DrawContext<'a, 'b, 'c, B: Backend> {
    device: &'c B::Device,
    render_pass: &'c B::RenderPass,
    encoder: &'a mut RenderPassInlineEncoder<'b, B, Primary>,
    viewport: &'c Viewport,
    update_viewport: bool,
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
            },
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

/// Picks a memory type staisfying `requirements` with `properties`,
/// or returns `None` if none could be found.
pub fn select_memory_type<I: IntoIterator<Item = Requirements>>(
    memory_types: &[MemoryType],
    requirements: I,
    properties: Properties,
) -> Option<MemoryTypeId> {
    let type_mask =
        requirements.into_iter().fold(!0, |mask, req| req.type_mask & mask);
    memory_types
        .iter()
        .enumerate()
        .find(|(id, mem)| {
            type_mask & (1u64 << id) != 0 && mem.properties.contains(properties)
        })
        .map(|(id, _)| MemoryTypeId(id))
}

impl<B: Backend> QueueGroups<B> {
    /// Gets the main graphics command queue.
    ///
    /// This is always the first queue for the graphics queue group.
    pub fn graphics_queue(
        &mut self,
    ) -> &mut CommandQueue<B, gfx_hal::Graphics> {
        match self {
            QueueGroups::Single(graphics) => &mut graphics.queues[0],
            QueueGroups::Separate {
                graphics,
                ..
            } => &mut graphics.queues[0],
        }
    }

    /// Gets the main transfer command queue.
    ///
    /// If there is not a separate transfer queue, this is the second
    /// graphics queue. If there is only one graphics queue, it just
    /// returns the same queue as a call to `graphics_queue`
    pub fn transfer_queue(
        &mut self,
    ) -> &mut CommandQueue<B, gfx_hal::Transfer> {
        match self {
            QueueGroups::Single(graphics) => {
                if graphics.queues.len() > 1 {
                    graphics.queues[1].downgrade()
                } else {
                    graphics.queues[0].downgrade()
                }
            },
            QueueGroups::Separate {
                transfer,
                ..
            } => &mut transfer.queues[0],
        }
    }

    /// Creates a command pool with graphics capability.
    pub fn create_graphics_command_pool(
        &self,
        device: &B::Device,
        flags: CommandPoolCreateFlags,
        max_buffers: usize,
    ) -> CommandPool<B, gfx_hal::Graphics> {
        let queue_group = match self {
            QueueGroups::Single(graphics) => graphics,
            QueueGroups::Separate {
                graphics,
                ..
            } => graphics,
        };
        device
            .create_command_pool_typed(queue_group, flags, max_buffers)
            .unwrap()
    }

    /// Creates a command pool with transfer capability.
    pub fn create_transfer_command_pool(
        &self,
        device: &B::Device,
        flags: CommandPoolCreateFlags,
        max_buffers: usize,
    ) -> CommandPool<B, gfx_hal::Transfer> {
        match self {
            // Waiting on https://github.com/gfx-rs/gfx/issues/2476 to remove unsafe.
            QueueGroups::Single(graphics) => unsafe {
                CommandPool::new(
                    device
                        .create_command_pool(graphics.family(), flags)
                        .unwrap(),
                )
            },
            QueueGroups::Separate {
                transfer,
                ..
            } => {
                device
                    .create_command_pool_typed(transfer, flags, max_buffers)
                    .unwrap()
            },
        }
    }
}

impl<B: Backend> Graphics<B> {
    pub fn new<I: Instance<Backend = B>>(
        instance: &I,
        mut surface: B::Surface,
        imgui: &mut ImGui,
        present_mode: PresentMode,
    ) -> Graphics<B> {
        let mut adapters = instance.enumerate_adapters().into_iter();

        // Pick the first adapter with a graphics queue family.
        let (adapter, device, mut queue_groups) = loop {
            let adapter = adapters.next().expect("No suitable adapter found");
            // Try to get a separate graphics and transfer queue,
            // since that's apparently faster.
            let transfer = adapter
                .queue_families
                .iter()
                .find(|family| family.queue_type() == QueueType::Transfer);
            let graphics = adapter.queue_families.iter().find(|family| {
                family.supports_graphics() &&
                    surface.supports_queue_family(family)
            });
            match (transfer, graphics) {
                (_, None) => continue, // No graphics queue.
                (None, Some(graphics)) => {
                    // No transfer queue.
                    let mut gpu = adapter
                        .physical_device
                        .open(&[(&graphics, &[1.0])])
                        .unwrap();
                    let queue_group = gpu
                        .queues
                        .take::<gfx_hal::Graphics>(graphics.id())
                        .unwrap();
                    break (
                        adapter,
                        gpu.device,
                        QueueGroups::Single(queue_group),
                    );
                },
                (Some(transfer), Some(graphics)) => {
                    // Separate transfer and graphics queues.
                    let mut gpu = adapter
                        .physical_device
                        .open(&[(&graphics, &[1.0]), (&transfer, &[0.0])])
                        .unwrap();
                    let graphics = gpu
                        .queues
                        .take::<gfx_hal::Graphics>(graphics.id())
                        .unwrap();
                    let transfer = gpu
                        .queues
                        .take::<gfx_hal::Transfer>(transfer.id())
                        .unwrap();
                    break (
                        adapter,
                        gpu.device,
                        QueueGroups::Separate {
                            graphics,
                            transfer,
                        },
                    );
                },
            }
        };
        let physical_device = &adapter.physical_device;
        let memory_types = physical_device.memory_properties().memory_types;

        let transfer_fence = device.create_fence(false).unwrap();

        let mut transfer_command_pool = queue_groups
            .create_transfer_command_pool(
                &device,
                CommandPoolCreateFlags::TRANSIENT,
                16,
            );

        // Create global UBO
        let global_ubo = device
            .create_buffer(
                GLOBAL_UBO_SIZE,
                buffer::Usage::TRANSFER_DST | buffer::Usage::UNIFORM,
            )
            .unwrap();
        let requirements = device.get_buffer_requirements(&global_ubo);
        let memory_type = select_memory_type(
            &memory_types,
            Some(requirements),
            Properties::DEVICE_LOCAL,
        )
        .expect("can't find memory type for global uniform buffer");
        let global_ubo_memory =
            device.allocate_memory(memory_type, requirements.size).unwrap();
        let global_ubo = device
            .bind_buffer_memory(&global_ubo_memory, 0, global_ubo)
            .unwrap();

        // Determine image capabilities and color format
        let (_, formats, _) = surface.compatibility(physical_device);
        let color_format = formats.map_or(Format::Rgba8Unorm, |formats| {
            formats
                .iter()
                .find(|format| format.base_format().1 == ChannelType::Unorm)
                .cloned()
                .unwrap_or(formats[0])
        });

        let render_pass = {
            let color_attachment = Attachment {
                format: Some(color_format),
                samples: 1,
                ops: AttachmentOps::new(
                    AttachmentLoadOp::Clear,
                    AttachmentStoreOp::Store,
                ),
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
                stages: PipelineStage::COLOR_ATTACHMENT_OUTPUT..
                    PipelineStage::COLOR_ATTACHMENT_OUTPUT,
                accesses: image::Access::empty()..
                    (image::Access::COLOR_ATTACHMENT_READ |
                        image::Access::COLOR_ATTACHMENT_WRITE),
            };

            device
                .create_render_pass(
                    &[color_attachment],
                    &[subpass],
                    &[dependency],
                )
                .unwrap()
        };

        let imgui_renderer = imgui_gfx_hal::Renderer::new(
            imgui,
            &device,
            physical_device,
            &render_pass,
            0,
            MAX_FRAMES,
            &mut transfer_command_pool,
            &mut queue_groups.transfer_queue(),
        )
        .unwrap();

        // TODO: figure out pool size
        let descriptor_pool = device
            .create_descriptor_pool(
                1,
                &[DescriptorRangeDesc {
                    ty: DescriptorType::UniformBuffer,
                    count: 1,
                }],
            )
            .unwrap();

        // TODO: this is ugly!
        let global_ubo_update_semaphore = device.create_semaphore().unwrap();
        let image_available_semaphores = (0..MAX_FRAMES)
            .map(|_| device.create_semaphore().unwrap())
            .collect();
        let frame_finished_semaphores = (0..MAX_FRAMES)
            .map(|_| device.create_semaphore().unwrap())
            .collect();
        let frame_fences = (0..MAX_FRAMES)
            .map(|_| device.create_fence(true).unwrap())
            .collect();
        let frame_command_pools = (0..MAX_FRAMES)
            .map(|_| {
                queue_groups.create_graphics_command_pool(
                    &device,
                    CommandPoolCreateFlags::TRANSIENT,
                    4,
                )
            })
            .collect();

        let swapchain_state = SwapchainState::new(
            &device,
            physical_device,
            &mut surface,
            &render_pass,
            color_format,
            present_mode,
            None,
        );

        Graphics {
            surface,
            adapter,
            memory_types,
            device,
            queue_groups,
            transfer_command_pool,
            transfer_fence,
            frame_command_pools,
            swapchain_state,
            render_pass,
            image_available_semaphores,
            frame_finished_semaphores,
            global_ubo_update_semaphore,
            frame_fences,
            descriptor_pool,
            global_ubo,
            global_ubo_memory,
            imgui_renderer,
            color_format,
            current_frame: 0,
            swapchain_update: false,
            viewport_update: false,
            first_frame: true,
            present_mode,
        }
    }

    pub fn set_present_mode(&mut self, present_mode: PresentMode) {
        self.present_mode = present_mode;
        self.swapchain_update = true;
    }

    /// Waits until the buffers for a new frame open up.
    ///
    /// This is useful to avoid input lag, since ideally inputs will
    /// be processed right before rendering, so delaying inside
    /// `draw_frame` is undesirable.
    pub fn wait_for_frame(&self) {
        let frame_fence = &self.frame_fences[self.current_frame];
        self.device.wait_for_fence(frame_fence, !0).unwrap();
    }

    pub fn draw_frame<F: FnOnce(DrawContext<B>)>(
        &mut self,
        ui: Ui,
        draw_fn: F,
    ) -> Result<(), ()> {
        // Frame specific resources...
        let frame_fence = &self.frame_fences[self.current_frame];
        let image_available_semaphore =
            &self.image_available_semaphores[self.current_frame];
        let frame_finished_semaphore =
            &self.frame_finished_semaphores[self.current_frame];

        // Make sure there are no more than MAX_FRAMES frames in flight.
        // TODO: somehow check that `wait_for_frame` has already been run.
        self.device.wait_for_fence(frame_fence, !0).unwrap();
        self.device.reset_fence(frame_fence).unwrap();
        self.frame_command_pools[self.current_frame].reset();

        if self.swapchain_update || self.viewport_update {
            let old_viewport = self.swapchain_state.viewport.clone();

            {
                // TODO: this is dumb and bad and I want partial borrowing
                let &mut Graphics {
                    ref device,
                    ref adapter,
                    ref mut surface,
                    ref render_pass,
                    ref color_format,
                    ref present_mode,
                    ref mut swapchain_state,
                    ..
                } = self;
                device.wait_idle().unwrap();
                take_mut::take(swapchain_state, |old| {
                    SwapchainState::new(
                        device,
                        &adapter.physical_device,
                        surface,
                        render_pass,
                        *color_format,
                        *present_mode,
                        Some(old),
                    )
                });
            }

            if self.swapchain_state.viewport != old_viewport {
                self.viewport_update = true;
            }
        }

        let ubo_update = self.first_frame || self.viewport_update;
        if ubo_update {
            // Update the global UBO.
            // self.transfer_command_pool.reset();
            let submit = {
                let mut cbuf =
                    self.transfer_command_pool.acquire_command_buffer(false);

                let (width, height) = (
                    f32::from(self.swapchain_state.viewport.rect.w),
                    f32::from(self.swapchain_state.viewport.rect.h),
                );
                let scale = if height < width {
                    [height / width, 1.0]
                } else {
                    [1.0, width / height]
                };
                let data = GlobalUbo {
                    scale,
                };
                let data: [u8; 4 * 2] = unsafe { mem::transmute(data) };

                cbuf.update_buffer(&self.global_ubo, 0, &data);
                let barrier = Barrier::Buffer {
                    states: buffer::Access::TRANSFER_WRITE..
                        buffer::Access::empty(),
                    target: &self.global_ubo,
                };
                cbuf.pipeline_barrier(
                    PipelineStage::TRANSFER..PipelineStage::BOTTOM_OF_PIPE,
                    Dependencies::empty(),
                    &[barrier],
                );

                cbuf.finish()
            };
            let submission = Submission::new()
                .signal(&[&self.global_ubo_update_semaphore])
                .submit(Some(submit));
            self.queue_groups.transfer_queue().submit(submission, None);
        }

        // Get swapchain index
        let frame_index: SwapImageIndex = self
            .swapchain_state
            .swapchain
            .acquire_image(!0, FrameSync::Semaphore(image_available_semaphore))
            .unwrap();

        let submit = {
            let mut cbuf = self.frame_command_pools[self.current_frame]
                .acquire_command_buffer(false);
            {
                if ubo_update {
                    let barrier = Barrier::Buffer {
                        states: buffer::Access::empty()..
                            buffer::Access::SHADER_READ,
                        target: &self.global_ubo,
                    };
                    cbuf.pipeline_barrier(
                        PipelineStage::TOP_OF_PIPE..
                            PipelineStage::VERTEX_SHADER,
                        Dependencies::empty(),
                        &[barrier],
                    );
                }

                // TODO: multithread this, and possibly cache command buffers
                let mut encoder = cbuf.begin_render_pass_inline(
                    &self.render_pass,
                    &self.swapchain_state.framebuffers[frame_index as usize],
                    self.swapchain_state.viewport.rect,
                    &[ClearValue::Color(ClearColor::Float([
                        0.0, 0.0, 0.0, 1.0,
                    ]))],
                );

                {
                    let ctx = DrawContext {
                        device: &self.device,
                        render_pass: &self.render_pass,
                        encoder: &mut encoder,
                        viewport: &self.swapchain_state.viewport,
                        update_viewport: self.viewport_update,
                    };
                    draw_fn(ctx);
                }

                self.imgui_renderer
                    .render(
                        ui,
                        self.current_frame,
                        &mut encoder,
                        &self.device,
                        &self.adapter.physical_device,
                    )
                    .unwrap();
            }
            cbuf.finish()
        };

        let mut submission = Submission::new()
            .wait_on(&[(
                image_available_semaphore,
                PipelineStage::COLOR_ATTACHMENT_OUTPUT,
            )])
            .signal(&[frame_finished_semaphore])
            .submit(Some(submit));
        if ubo_update {
            submission = submission.wait_on(&[(
                &self.global_ubo_update_semaphore,
                PipelineStage::VERTEX_SHADER,
            )])
        }
        self.queue_groups
            .graphics_queue()
            .submit(submission, Some(frame_fence));

        self.current_frame = (self.current_frame + 1) % MAX_FRAMES;
        self.swapchain_update = false;
        self.viewport_update = false;
        self.first_frame = false;

        if self
            .swapchain_state
            .swapchain
            .present(
                &mut self.queue_groups.graphics_queue(),
                frame_index,
                [frame_finished_semaphore].iter().cloned(),
            )
            .is_err()
        {
            error!("error occurred while presenting swapchain");
            // TODO: detect if it's a bad swapchain error or not
            self.swapchain_update = true;
            return Err(());
        }

        Ok(())
    }

    pub fn destroy(self) {
        let Graphics {
            device,
            transfer_command_pool,
            transfer_fence,
            frame_command_pools,
            render_pass,
            frame_finished_semaphores,
            image_available_semaphores,
            global_ubo_update_semaphore,
            frame_fences,
            descriptor_pool,
            swapchain_state,
            global_ubo,
            global_ubo_memory,
            imgui_renderer,
            ..
        } = self;

        device.wait_idle().unwrap();
        device.destroy_fence(transfer_fence);
        swapchain_state.destroy(&device);
        device.destroy_command_pool(transfer_command_pool.into_raw());
        for command_pool in frame_command_pools.into_iter() {
            device.destroy_command_pool(command_pool.into_raw());
        }
        for fence in frame_fences.into_iter() {
            device.destroy_fence(fence);
        }
        device.destroy_semaphore(global_ubo_update_semaphore);
        for semaphore in frame_finished_semaphores.into_iter() {
            device.destroy_semaphore(semaphore);
        }
        for semaphore in image_available_semaphores.into_iter() {
            device.destroy_semaphore(semaphore);
        }
        device.destroy_descriptor_pool(descriptor_pool);
        device.destroy_render_pass(render_pass);
        device.destroy_buffer(global_ubo);
        device.free_memory(global_ubo_memory);
        imgui_renderer.destroy(&device);
    }
}

impl<B: Backend> SwapchainState<B> {
    fn new(
        device: &B::Device,
        physical_device: &B::PhysicalDevice,
        surface: &mut B::Surface,
        render_pass: &B::RenderPass,
        color_format: Format,
        present_mode: PresentMode,
        old: Option<SwapchainState<B>>,
    ) -> SwapchainState<B> {
        let (caps, ..) = surface.compatibility(physical_device);
        let extent = caps.current_extent.unwrap();
        assert!(caps.image_count.contains(&(MAX_FRAMES as u32)));
        let swapchain_config = SwapchainConfig {
            present_mode,
            image_count: MAX_FRAMES as u32,
            ..SwapchainConfig::from_caps(&caps, color_format, extent)
        };
        info!(
            "building swapchain at extent {},{}",
            extent.width, extent.height,
        );

        // Destroy the old buffers, but keep the swapchain itself around
        let old = old.map(|old| {
            for framebuffer in old.framebuffers {
                device.destroy_framebuffer(framebuffer);
            }
            for image_view in old.frame_views {
                device.destroy_image_view(image_view);
            }
            old.swapchain
        });

        let (swapchain, backbuffer) =
            device.create_swapchain(surface, swapchain_config, old).unwrap();

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
                            .create_framebuffer(
                                &render_pass,
                                vec![image_view],
                                extent.to_extent(),
                            )
                            .unwrap()
                    })
                    .collect();

                (image_views, fbos)
            },
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
