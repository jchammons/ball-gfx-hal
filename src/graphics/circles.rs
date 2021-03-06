use crate::graphics::{create_buffer, DrawContext, Graphics, GLOBAL_UBO_SIZE};
use gfx_hal::{
    buffer::{Access, Usage},
    command::{BufferCopy, OneShot},
    format::Format,
    memory::{Barrier, Dependencies, Properties},
    pass::Subpass,
    pso::{
        AttributeDesc,
        BlendState,
        ColorBlendDesc,
        ColorMask,
        Descriptor,
        DescriptorSetLayoutBinding,
        DescriptorSetWrite,
        DescriptorType,
        Element,
        EntryPoint,
        Face,
        GraphicsPipelineDesc,
        GraphicsShaderSet,
        PipelineStage,
        Rasterizer,
        ShaderStageFlags,
        Specialization,
        VertexBufferDesc,
    },
    Backend,
    DescriptorPool,
    Device,
    Primitive,
};
use nalgebra::Point2;
use palette::LinSrgb;
use std::mem;

#[derive(Copy, Clone, Debug)]
#[repr(C, packed)]
struct Vertex {
    position: [f32; 2],
}

#[derive(Copy, Clone, Debug)]
pub struct Circle {
    pub center: Point2<f32>,
    pub radius: f32,
    pub color: LinSrgb,
}

const VERTS: [Vertex; 4] = [
    Vertex {
        position: [-1.0, -1.0],
    },
    Vertex {
        position: [1.0, -1.0],
    },
    Vertex {
        position: [-1.0, 1.0],
    },
    Vertex {
        position: [1.0, 1.0],
    },
];

pub struct CircleRenderer<B: Backend> {
    vertex_buffer: B::Buffer,
    vertex_memory: B::Memory,
    pipeline_layout: B::PipelineLayout,
    descriptor_set_layout: B::DescriptorSetLayout,
    global_ubo_descriptor_set: B::DescriptorSet,
    vs_module: B::ShaderModule,
    fs_module: B::ShaderModule,
    pipeline: B::GraphicsPipeline,
}

impl<B: Backend> CircleRenderer<B> {
    pub fn new(graphics: &mut Graphics<B>) -> CircleRenderer<B> {
        // Create vertex buffer.
        let size = 4 * mem::size_of::<Vertex>() as u64;
        let (vertex_buffer, vertex_memory, _) = unsafe {
            create_buffer::<B>(
                &graphics.device,
                &graphics.memory_types,
                Properties::DEVICE_LOCAL,
                Usage::TRANSFER_DST | Usage::VERTEX,
                size,
            )
        };

        // Create staging buffer.
        let (staging_buffer, staging_memory, _) = unsafe {
            create_buffer::<B>(
                &graphics.device,
                &graphics.memory_types,
                Properties::CPU_VISIBLE,
                Usage::TRANSFER_SRC,
                size,
            )
        };

        // Copy vertices to the staging buffer.
        unsafe {
            let mut map = graphics
                .device
                .acquire_mapping_writer(&staging_memory, 0..size)
                .unwrap();
            map.clone_from_slice(&VERTS);
            graphics.device.release_mapping_writer(map).unwrap();
        }

        // Copy staging buffer to vertex buffer.
        // TODO: handle unified graphics/transfer queue differently.
        let mut cmd_buffer =
            graphics.transfer_command_pool.acquire_command_buffer::<OneShot>();

        unsafe {
            cmd_buffer.begin();

            cmd_buffer.copy_buffer(
                &staging_buffer,
                &vertex_buffer,
                &[BufferCopy {
                    src: 0,
                    dst: 0,
                    size,
                }],
            );

            let barrier = Barrier::whole_buffer(
                &vertex_buffer,
                Access::TRANSFER_WRITE..Access::VERTEX_BUFFER_READ,
            );
            cmd_buffer.pipeline_barrier(
                PipelineStage::TRANSFER..PipelineStage::VERTEX_INPUT,
                Dependencies::empty(),
                &[barrier],
            );

            cmd_buffer.finish();

            graphics.device.reset_fence(&graphics.transfer_fence).unwrap();
            graphics.queue_group.queues[0].submit_nosemaphores(
                Some(&cmd_buffer),
                Some(&graphics.transfer_fence),
            );
        }

        // Load shaders.
        let vs_module = {
            let spirv = include_bytes!(concat!(
                env!("OUT_DIR"),
                "/shaders/circle.vert.spirv"
            ));
            unsafe { graphics.device.create_shader_module(spirv).unwrap() }
        };
        let fs_module = {
            let spirv = include_bytes!(concat!(
                env!("OUT_DIR"),
                "/shaders/circle.frag.spirv"
            ));
            unsafe { graphics.device.create_shader_module(spirv).unwrap() }
        };

        // Create descriptor set layout and descriptor set for global
        // UBO.
        // TODO: maybe this should be in graphics?
        let global_ubo_layout_binding = DescriptorSetLayoutBinding {
            binding: 0,
            ty: DescriptorType::UniformBuffer,
            count: 1,
            stage_flags: ShaderStageFlags::ALL,
            immutable_samplers: false,
        };
        let descriptor_set_layout = unsafe {
            graphics
                .device
                .create_descriptor_set_layout(&[global_ubo_layout_binding], &[])
                .unwrap()
        };
        let global_ubo_descriptor_set = unsafe {
            graphics
                .descriptor_pool
                .allocate_set(&descriptor_set_layout)
                .unwrap()
        };
        let write = DescriptorSetWrite {
            set: &global_ubo_descriptor_set,
            binding: 0,
            array_offset: 0,
            descriptors: &[Descriptor::Buffer(
                &graphics.global_ubo,
                Some(0)..Some(GLOBAL_UBO_SIZE),
            )],
        };
        unsafe {
            graphics.device.write_descriptor_sets(Some(write));
        }

        // Create pipeline for circle rendering.
        let pipeline_layout = unsafe {
            graphics
                .device
                .create_pipeline_layout(
                    Some(&descriptor_set_layout),
                    &[(ShaderStageFlags::GRAPHICS, 0..8)],
                )
                .unwrap()
        };

        let vs_entry = EntryPoint {
            entry: "main",
            module: &vs_module,
            specialization: Specialization::default(),
        };
        let fs_entry = EntryPoint {
            entry: "main",
            module: &fs_module,
            specialization: Specialization::default(),
        };

        let shader_entries = GraphicsShaderSet {
            vertex: vs_entry,
            hull: None,
            domain: None,
            geometry: None,
            fragment: Some(fs_entry),
        };

        let subpass = Subpass {
            index: 0,
            main_pass: &graphics.render_pass,
        };

        let mut pipeline_desc = GraphicsPipelineDesc::new(
            shader_entries,
            Primitive::TriangleStrip,
            Rasterizer {
                cull_face: Face::NONE,
                ..Rasterizer::FILL
            },
            &pipeline_layout,
            subpass,
        );

        // Enable blending (for fake AA).
        pipeline_desc
            .blender
            .targets
            .push(ColorBlendDesc(ColorMask::ALL, BlendState::ALPHA));

        pipeline_desc.vertex_buffers.push(VertexBufferDesc {
            binding: 0,
            stride: mem::size_of::<Vertex>() as u32,
            rate: 0,
        });

        pipeline_desc.attributes.push(AttributeDesc {
            location: 0,
            binding: 0,
            element: Element {
                format: Format::Rg32Float,
                offset: 0,
            },
        });

        let pipeline = unsafe {
            graphics
                .device
                .create_graphics_pipeline(&pipeline_desc, None)
                .unwrap()
        };

        // When transfer is finished, delete the staging buffers.
        unsafe {
            graphics
                .device
                .wait_for_fence(&graphics.transfer_fence, !0)
                .unwrap();
            graphics.device.destroy_buffer(staging_buffer);
            graphics.device.free_memory(staging_memory);
        }

        CircleRenderer {
            vertex_buffer,
            vertex_memory,
            pipeline_layout,
            descriptor_set_layout,
            global_ubo_descriptor_set,
            vs_module,
            fs_module,
            pipeline,
        }
    }

    pub fn draw<I: IntoIterator<Item = Circle>>(
        &mut self,
        ctx: &mut DrawContext<B>,
        circles: I,
    ) {
        // TODO: re-use command buffers
        unsafe {
            ctx.encoder.bind_vertex_buffers(
                0,
                [(&self.vertex_buffer, 0)].iter().cloned(),
            );
            ctx.encoder.bind_graphics_pipeline(&self.pipeline);
            ctx.encoder.set_viewports(0, Some(ctx.viewport));
            ctx.encoder.set_scissors(0, Some(&ctx.viewport.rect));
            ctx.encoder.bind_graphics_descriptor_sets(
                &self.pipeline_layout,
                0,
                Some(&self.global_ubo_descriptor_set),
                None as Option<u32>,
            );
            for circle in circles {
                let push_constants = [
                    circle.radius,
                    0.0, // padding
                    circle.center.x,
                    circle.center.y,
                    circle.color.red,
                    circle.color.green,
                    circle.color.blue,
                    1.0,
                ];
                let push_constants: [u32; 8] = mem::transmute(push_constants);
                ctx.encoder.push_graphics_constants(
                    &self.pipeline_layout,
                    ShaderStageFlags::GRAPHICS,
                    0,
                    &push_constants,
                );
                ctx.encoder.draw(0..4, 0..1);
            }
        }
    }

    pub fn destroy(self, graphics: &mut Graphics<B>) {
        graphics.device.wait_idle().unwrap();
        unsafe {
            graphics.device.destroy_buffer(self.vertex_buffer);
            graphics.device.free_memory(self.vertex_memory);
            graphics.device.destroy_pipeline_layout(self.pipeline_layout);
            graphics.device.destroy_graphics_pipeline(self.pipeline);
            graphics.device.destroy_shader_module(self.vs_module);
            graphics.device.destroy_shader_module(self.fs_module);
            graphics
                .device
                .destroy_descriptor_set_layout(self.descriptor_set_layout);
        }
    }
}
