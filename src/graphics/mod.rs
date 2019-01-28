use arrayvec::ArrayVec;
use gfx_hal::{
    adapter::DeviceType,
    buffer,
    command::{
        ClearColor,
        ClearValue,
        CommandBuffer,
        OneShot,
        Primary,
        RenderPassInlineEncoder,
    },
    error::DeviceCreationError,
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
    window::CompositeAlpha,
    Adapter,
    Backbuffer,
    Backend,
    CommandPool,
    Device,
    FrameSync,
    Instance,
    MemoryType,
    MemoryTypeId,
    PhysicalDevice,
    PresentMode,
    QueueFamily,
    QueueGroup,
    Submission,
    Surface,
    SwapImageIndex,
    Swapchain,
    SwapchainConfig,
};
use imgui::{ImGui, Ui};
use imgui_gfx_hal;
use itertools::Itertools;
use log::{debug, error, info};
use smallvec::SmallVec;
use std::cmp::Ordering;
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

pub struct Graphics<B: Backend> {
    surface: B::Surface,
    adapter: Adapter<B>,
    device: B::Device,
    memory_types: Vec<MemoryType>,
    queue_group: QueueGroup<B, gfx_hal::Graphics>,
    transfer_command_pool: CommandPool<B, gfx_hal::Graphics>,
    frame_command_pools:
        ArrayVec<[CommandPool<B, gfx_hal::Graphics>; MAX_FRAMES]>,
    frame_cmd_buffers: ArrayVec<
        [CommandBuffer<B, gfx_hal::Graphics, OneShot, Primary>; MAX_FRAMES],
    >,
    global_ubo_update_command_pool: CommandPool<B, gfx_hal::Graphics>,
    global_ubo_update_cmd_buffer:
        CommandBuffer<B, gfx_hal::Graphics, OneShot, Primary>,
    swapchain_state: SwapchainState<B>,
    render_pass: B::RenderPass,
    global_ubo: B::Buffer,
    global_ubo_memory: B::Memory,
    descriptor_pool: B::DescriptorPool,
    image_available_semaphores: ArrayVec<[B::Semaphore; MAX_FRAMES]>,
    frame_finished_semaphores: ArrayVec<[B::Semaphore; MAX_FRAMES]>,
    global_ubo_update_fence: B::Fence,
    frame_fences: ArrayVec<[B::Fence; MAX_FRAMES]>,
    transfer_fence: B::Fence,
    imgui_renderer: imgui_gfx_hal::Renderer<B>,
    color_format: Format,
    present_mode: PresentMode,
    supported_present_modes: Vec<PresentMode>,
    cleanup: ArrayVec<[SmallVec<[Cleanup<B>; 3]>; MAX_FRAMES]>,
    current_frame: usize,
    swapchain_update: bool,
    viewport_update: bool,
    first_frame: bool,
}

/// Resources to clean up after a particular frame is done.
///
/// These are created when the window resizes, since the old resources
/// can't be destroyed until the last frame to use them has rendered.
pub struct Cleanup<B: Backend> {
    framebuffers: Vec<B::Framebuffer>,
    frame_views: Vec<B::ImageView>,
}

pub struct DrawContext<'a, 'b, 'c, B: Backend> {
    encoder: &'a mut RenderPassInlineEncoder<'b, B>,
    viewport: &'c Viewport,
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

    pub fn trigger_capture(_: &mut RenderDoc, n_frames: u32) {}
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
        .position(|(id, mem)| {
            type_mask & (1u64 << id) != 0 && mem.properties.contains(properties)
        })
        .map(|id| id.into())
}

/// Creates a buffer and binds memory to it.
///
/// Returns the buffer, bound memory, and the allocated buffer size.
pub unsafe fn create_buffer<B: Backend>(
    device: &B::Device,
    memory_types: &[MemoryType],
    properties: Properties,
    usage: buffer::Usage,
    size: u64,
) -> (B::Buffer, B::Memory, u64) {
    let mut buffer = device.create_buffer(size, usage).unwrap();
    let requirements = device.get_buffer_requirements(&buffer);
    let memory_type =
        select_memory_type(&memory_types, Some(requirements), properties)
            .expect("can't find memory type");
    let memory =
        device.allocate_memory(memory_type, requirements.size).unwrap();
    device.bind_buffer_memory(&memory, 0, &mut buffer).unwrap();

    (buffer, memory, requirements.size)
}

impl<B: Backend> Graphics<B> {
    pub fn new<I: Instance<Backend = B>>(
        instance: &I,
        mut surface: B::Surface,
        imgui: &mut ImGui,
    ) -> Graphics<B> {
        let mut adapters =
            instance.enumerate_adapters().into_iter().sorted_by(|a, b| {
                // Prefer discrete gpus to everything else, and everything else
                // to software rendering.
                match (&a.info.device_type, &b.info.device_type) {
                    (DeviceType::DiscreteGpu, _) => Ordering::Less,
                    (_, DeviceType::DiscreteGpu) => Ordering::Greater,
                    (DeviceType::Cpu, _) => Ordering::Greater,
                    (_, DeviceType::Cpu) => Ordering::Less,
                    _ => Ordering::Equal,
                }
            });

        // Pick the first adapter with a graphics queue family.
        let (adapter, device, mut queue_group) = loop {
            let adapter = adapters.next().expect("No suitable adapter found");
            match adapter.open_with::<_, gfx_hal::Graphics>(1, |family| {
                family.supports_graphics() &&
                    surface.supports_queue_family(family)
            }) {
                Ok((device, queue_group)) => {
                    break (adapter, device, queue_group);
                },
                // These errors mean we just give up on this adapter.
                Err(DeviceCreationError::MissingExtension) |
                Err(DeviceCreationError::MissingFeature) => (),
                // These errors are more serious and should be reported.
                Err(err) => {
                    error!(
                        "error opening device from adapter '{}': {}",
                        adapter.info.name, err
                    )
                },
            }
        };
        info!("selected adapter '{}'", adapter.info.name);
        let physical_device = &adapter.physical_device;
        let memory_types = physical_device.memory_properties().memory_types;

        let transfer_fence = device.create_fence(false).unwrap();

        let mut transfer_command_pool = unsafe {
            device
                .create_command_pool_typed(
                    &queue_group,
                    CommandPoolCreateFlags::TRANSIENT,
                )
                .unwrap()
        };

        // Create global UBO
        let (global_ubo, global_ubo_memory) = unsafe {
            let mut global_ubo = device
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
            device
                .bind_buffer_memory(&global_ubo_memory, 0, &mut global_ubo)
                .unwrap();
            (global_ubo, global_ubo_memory)
        };

        // Determine image capabilities and color format
        // TODO figure out what available present modes are
        let (_, formats, supported_present_modes, _) =
            surface.compatibility(physical_device);

        // Select present mode.
        let present_mode =
            if supported_present_modes.contains(&PresentMode::Mailbox) {
                // Use mailbox if available.
                PresentMode::Mailbox
            } else {
                // Otherwise default to immediate.
                PresentMode::Immediate
            };

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

            unsafe {
                device
                    .create_render_pass(
                        &[color_attachment],
                        &[subpass],
                        &[dependency],
                    )
                    .unwrap()
            }
        };

        let imgui_renderer = imgui_gfx_hal::Renderer::new(
            imgui,
            &device,
            physical_device,
            &render_pass,
            0,
            MAX_FRAMES,
            &mut transfer_command_pool,
            &mut queue_group.queues[0],
        )
        .unwrap();

        // TODO: figure out pool size
        let descriptor_pool = unsafe {
            device
                .create_descriptor_pool(
                    1,
                    &[DescriptorRangeDesc {
                        ty: DescriptorType::UniformBuffer,
                        count: 1,
                    }],
                )
                .unwrap()
        };

        // TODO: this is ugly!
        let image_available_semaphores = (0..MAX_FRAMES)
            .map(|_| device.create_semaphore().unwrap())
            .collect();
        let frame_finished_semaphores = (0..MAX_FRAMES)
            .map(|_| device.create_semaphore().unwrap())
            .collect();
        let frame_fences = (0..MAX_FRAMES)
            .map(|_| device.create_fence(true).unwrap())
            .collect();
        // Allocate a separate command pool for each frame, to allow
        // resetting the corresponding command buffers individually.
        let mut frame_command_pools: ArrayVec<[_; MAX_FRAMES]> = (0..
            MAX_FRAMES)
            .map(|_| {
                unsafe {
                    device
                        .create_command_pool_typed(
                            &queue_group,
                            CommandPoolCreateFlags::empty(),
                        )
                        .unwrap()
                }
            })
            .collect();
        // Allocate a command buffer for each frame.
        let frame_cmd_buffers = (0..MAX_FRAMES)
            .map(|frame| {
                frame_command_pools[frame].acquire_command_buffer::<OneShot>()
            })
            .collect();

        let mut global_ubo_update_command_pool = unsafe {
            device
                .create_command_pool_typed(
                    &queue_group,
                    CommandPoolCreateFlags::empty(),
                )
                .unwrap()
        };
        let global_ubo_update_cmd_buffer =
            global_ubo_update_command_pool.acquire_command_buffer::<OneShot>();
        let global_ubo_update_fence = device.create_fence(true).unwrap();

        let cleanup = (0..MAX_FRAMES).map(|_| SmallVec::new()).collect();

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
            queue_group,
            transfer_command_pool,
            transfer_fence,
            frame_command_pools,
            frame_cmd_buffers,
            global_ubo_update_command_pool,
            global_ubo_update_cmd_buffer,
            swapchain_state,
            render_pass,
            image_available_semaphores,
            frame_finished_semaphores,
            global_ubo_update_fence,
            frame_fences,
            descriptor_pool,
            global_ubo,
            global_ubo_memory,
            imgui_renderer,
            color_format,
            cleanup,
            current_frame: 0,
            swapchain_update: false,
            viewport_update: false,
            first_frame: true,
            present_mode,
            supported_present_modes,
        }
    }

    pub fn supported_present_modes(&self) -> &[PresentMode] {
        &self.supported_present_modes
    }

    pub fn present_mode(&self) -> PresentMode {
        self.present_mode
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
        unsafe { self.device.wait_for_fence(frame_fence, !0).unwrap() };
    }

    pub fn draw_frame<F: FnOnce(DrawContext<B>)>(
        &mut self,
        ui: Ui,
        draw_fn: F,
    ) -> Result<(), ()> {
        let queue = &mut self.queue_group.queues[0];

        // Frame specific resources...
        let frame_fence = &self.frame_fences[self.current_frame];
        let image_available_semaphore =
            &self.image_available_semaphores[self.current_frame];
        let frame_finished_semaphore =
            &self.frame_finished_semaphores[self.current_frame];

        let mut cleanup = None;
        if self.swapchain_update || self.viewport_update {
            let old_viewport = self.swapchain_state.viewport.clone();

            {
                // TODO: this is dumb and bad and I want partial
                // borrowing
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
                take_mut::take(swapchain_state, |old| {
                    let SwapchainState {
                        framebuffers,
                        frame_views,
                        swapchain,
                        ..
                    } = old;
                    // Clean up the resources from the old swapchain
                    // when the last frame to use them is done.
                    cleanup = Some(Cleanup {
                        framebuffers,
                        frame_views,
                    });
                    SwapchainState::new(
                        device,
                        &adapter.physical_device,
                        surface,
                        render_pass,
                        *color_format,
                        *present_mode,
                        Some(swapchain),
                    )
                });
            }

            if self.swapchain_state.viewport != old_viewport {
                self.viewport_update = true;
            }
        }

        if self.first_frame || self.viewport_update {
            // Update the global UBO.
            unsafe {
                // Make sure the command pool doesn't get reset while the
                // last update is still running. It's okay to block on
                // this since it doesn't happen very often.
                self.device
                    .wait_for_fence(&self.global_ubo_update_fence, !0)
                    .unwrap();
                self.device.reset_fence(&self.global_ubo_update_fence).unwrap();
                // Reset the command buffer, so that it can be
                // rerecorded with the new data.
                self.global_ubo_update_command_pool.reset();
                let cmd_buffer = &mut self.global_ubo_update_cmd_buffer;

                cmd_buffer.begin();

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
                // Double unsafe!
                let data: [u8; 4 * 2] = mem::transmute(data);

                // Barrier to prevent updating the ubo while it's
                // still in use in a previous frame.
                let barrier = Barrier::whole_buffer(
                    &self.global_ubo,
                    buffer::Access::CONSTANT_BUFFER_READ..
                        buffer::Access::TRANSFER_WRITE,
                );
                // As of right now, the vertex shader reads the ubo,
                // but the fragment shader does not. If that changes,
                // this needs to be changed to VERTEX_SHADER | FRAGMENT_SHADER.
                cmd_buffer.pipeline_barrier(
                    PipelineStage::VERTEX_SHADER..PipelineStage::TRANSFER,
                    Dependencies::empty(),
                    &[barrier],
                );
                cmd_buffer.update_buffer(&self.global_ubo, 0, &data);

                // Barrier to prevent future frames from reading the
                // ubo until the update finishes.
                let barrier = Barrier::whole_buffer(
                    &self.global_ubo,
                    buffer::Access::TRANSFER_WRITE..
                        buffer::Access::CONSTANT_BUFFER_READ,
                );
                cmd_buffer.pipeline_barrier(
                    PipelineStage::TRANSFER..PipelineStage::VERTEX_SHADER,
                    Dependencies::empty(),
                    &[barrier],
                );

                cmd_buffer.finish();

                queue.submit_nosemaphores(
                    Some(&*cmd_buffer),
                    Some(&self.global_ubo_update_fence),
                );
            }
        }

        // Make sure there are no more than MAX_FRAMES frames in flight.
        unsafe {
            self.device.wait_for_fence(frame_fence, !0).unwrap();
            self.device.reset_fence(frame_fence).unwrap();
        }

        // Get swapchain index
        let frame_index: SwapImageIndex = unsafe {
            self.swapchain_state
                .swapchain
                .acquire_image(
                    !0,
                    FrameSync::Semaphore(image_available_semaphore),
                )
                .unwrap()
        };

        unsafe {
            self.frame_command_pools[self.current_frame].reset();
        }
        let cmd_buffer = &mut self.frame_cmd_buffers[self.current_frame];

        // Clean up any old resources that were waiting on this frame.
        for cleanup in self.cleanup[self.current_frame].drain() {
            cleanup.destroy(&self.device);
        }

        unsafe {
            cmd_buffer.begin();

            {
                // TODO: multithread this, and possibly cache command
                // buffers
                let mut encoder = cmd_buffer.begin_render_pass_inline(
                    &self.render_pass,
                    &self.swapchain_state.framebuffers[frame_index as usize],
                    self.swapchain_state.viewport.rect,
                    &[ClearValue::Color(ClearColor::Float([
                        0.0, 0.0, 0.0, 1.0,
                    ]))],
                );

                {
                    let ctx = DrawContext {
                        encoder: &mut encoder,
                        viewport: &self.swapchain_state.viewport,
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

            cmd_buffer.finish();

            let submission = Submission {
                command_buffers: Some(&*cmd_buffer),
                wait_semaphores: Some((
                    image_available_semaphore,
                    PipelineStage::COLOR_ATTACHMENT_OUTPUT,
                )),
                signal_semaphores: Some(frame_finished_semaphore),
            };
            queue.submit(submission, Some(frame_fence));
        }

        if let Some(cleanup) = cleanup {
            self.cleanup[self.current_frame].push(cleanup);
        }

        self.current_frame = (self.current_frame + 1) % MAX_FRAMES;
        self.swapchain_update = false;
        self.viewport_update = false;
        self.first_frame = false;

        unsafe {
            if self
                .swapchain_state
                .swapchain
                .present(
                    queue,
                    frame_index,
                    [frame_finished_semaphore].iter().cloned(),
                )
                .is_err()
            {
                // TODO: detect if it's a bad swapchain error or not
                self.swapchain_update = true;
                return Err(());
            }
        }

        Ok(())
    }

    pub fn destroy(self) {
        let Graphics {
            device,
            transfer_command_pool,
            transfer_fence,
            frame_command_pools,
            global_ubo_update_command_pool,
            render_pass,
            frame_finished_semaphores,
            image_available_semaphores,
            global_ubo_update_fence,
            frame_fences,
            descriptor_pool,
            swapchain_state,
            global_ubo,
            global_ubo_memory,
            imgui_renderer,
            cleanup,
            ..
        } = self;

        device.wait_idle().unwrap();
        unsafe {
            for cleanups in cleanup.into_iter() {
                for cleanup in cleanups.into_iter() {
                    cleanup.destroy(&device);
                }
            }

            device.destroy_fence(transfer_fence);
            swapchain_state.destroy(&device);
            device.destroy_command_pool(transfer_command_pool.into_raw());
            device.destroy_command_pool(
                global_ubo_update_command_pool.into_raw(),
            );
            for command_pool in frame_command_pools.into_iter() {
                device.destroy_command_pool(command_pool.into_raw());
            }
            for fence in frame_fences.into_iter() {
                device.destroy_fence(fence);
            }
            device.destroy_fence(global_ubo_update_fence);
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
        }
        imgui_renderer.destroy(&device);
    }
}

impl<B: Backend> Cleanup<B> {
    /// Destroys all resources marked for cleanup.
    fn destroy(self, device: &B::Device) {
        let Cleanup {
            framebuffers,
            frame_views,
            ..
        } = self;
        unsafe {
            for framebuffer in framebuffers {
                device.destroy_framebuffer(framebuffer);
            }
            for image_view in frame_views {
                device.destroy_image_view(image_view);
            }
        }
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
        old: Option<B::Swapchain>,
    ) -> SwapchainState<B> {
        let (caps, ..) = surface.compatibility(physical_device);
        let extent = caps.current_extent.unwrap();
        assert!(caps.image_count.contains(&(MAX_FRAMES as u32)));
        let swapchain_config = SwapchainConfig {
            present_mode,
            composite_alpha: CompositeAlpha::Opaque,
            image_count: MAX_FRAMES as u32,
            ..SwapchainConfig::from_caps(&caps, color_format, extent)
        };
        debug!(
            "building swapchain at extent {},{}",
            extent.width, extent.height,
        );

        let (swapchain, backbuffer) = unsafe {
            device.create_swapchain(surface, swapchain_config, old).unwrap()
        };

        let (frame_views, framebuffers) = match backbuffer {
            Backbuffer::Images(images) => {
                let color_range = SubresourceRange {
                    aspects: Aspects::COLOR,
                    levels: 0..1,
                    layers: 0..1,
                };

                let image_views = images
                    .iter()
                    .map(|image| unsafe {
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
                    .map(|image_view| unsafe {
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
        unsafe {
            device.destroy_swapchain(self.swapchain);
            for framebuffer in self.framebuffers {
                device.destroy_framebuffer(framebuffer);
            }
            for image_view in self.frame_views {
                device.destroy_image_view(image_view);
            }
        }
    }
}
