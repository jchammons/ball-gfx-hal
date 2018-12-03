use crate::graphics::{select_memory_type, DrawContext, Graphics, GLOBAL_UBO_SIZE};
use cgmath::Point2;
use gfx_hal::{
    buffer::{Access, Usage},
    command::BufferCopy,
    format::Format,
    memory::{Barrier, Dependencies, Properties},
    pass::Subpass,
    pso::{
        AttributeDesc, BlendState, ColorBlendDesc, ColorMask, Descriptor,
        DescriptorSetLayoutBinding, DescriptorSetWrite, DescriptorType, Element, EntryPoint, Face,
        GraphicsPipelineDesc, GraphicsShaderSet, PipelineStage, Rasterizer, ShaderStageFlags,
        Specialization, VertexBufferDesc, Viewport,
    },
    Backend, DescriptorPool, Device, Primitive, Submission,
};
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
    vbo_memory: B::Memory,
    pipeline_layout: B::PipelineLayout,
    descriptor_set_layout: B::DescriptorSetLayout,
    global_ubo_descriptor_set: B::DescriptorSet,
    vs_module: B::ShaderModule,
    fs_module: B::ShaderModule,
    pipeline: B::GraphicsPipeline,
}

fn create_pipeline<'a, B: Backend>(
    device: &B::Device,
    vs_module: &'a B::ShaderModule,
    fs_module: &'a B::ShaderModule,
    pipeline_layout: &'a B::PipelineLayout,
    render_pass: &'a B::RenderPass,
    viewport: Viewport,
) -> B::GraphicsPipeline {
    let vs_entry = EntryPoint {
        entry: "main",
        module: vs_module,
        specialization: Specialization::default(),
    };
    let fs_entry = EntryPoint {
        entry: "main",
        module: fs_module,
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
        main_pass: render_pass,
    };

    let mut pipeline_desc = GraphicsPipelineDesc::new(
        shader_entries,
        Primitive::TriangleStrip,
        Rasterizer {
            cull_face: Face::NONE,
            ..Rasterizer::FILL
        },
        pipeline_layout,
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

    pipeline_desc.baked_states.viewport = Some(viewport.clone());
    pipeline_desc.baked_states.scissor = Some(viewport.rect);

    device
        .create_graphics_pipeline(&pipeline_desc, None)
        .unwrap()
}

impl<B: Backend> CircleRenderer<B> {
    pub fn new(graphics: &mut Graphics<B>) -> CircleRenderer<B> {
        // Create vertex buffer.
        let size = 4 * mem::size_of::<Vertex>() as u64;
        let vertex_buffer = graphics
            .device
            .create_buffer(size, Usage::TRANSFER_DST | Usage::VERTEX)
            .unwrap();
        let requirements = graphics.device.get_buffer_requirements(&vertex_buffer);
        let memory_type = select_memory_type(
            &graphics.memory_types,
            Some(requirements),
            Properties::DEVICE_LOCAL,
        )
        .expect("can't find memory type for vertex buffer");
        let vbo_memory = graphics
            .device
            .allocate_memory(memory_type, requirements.size)
            .unwrap();
        let vertex_buffer = graphics
            .device
            .bind_buffer_memory(&vbo_memory, 0, vertex_buffer)
            .unwrap();

        // Create staging buffer.
        let staging_buffer = graphics
            .device
            .create_buffer(size, Usage::TRANSFER_SRC)
            .unwrap();
        let requirements = graphics.device.get_buffer_requirements(&staging_buffer);
        let memory_type = select_memory_type(
            &graphics.memory_types,
            Some(requirements),
            Properties::CPU_VISIBLE,
        )
        .expect("can't find memory type for vertex staging buffer");
        let staging_memory = graphics
            .device
            .allocate_memory(memory_type, requirements.size)
            .unwrap();
        let staging_buffer = graphics
            .device
            .bind_buffer_memory(&staging_memory, 0, staging_buffer)
            .unwrap();

        // Copy vertices to the staging buffer.
        let mut map = graphics
            .device
            .acquire_mapping_writer(&staging_memory, 0..size)
            .unwrap();
        map.clone_from_slice(&VERTS);
        graphics.device.release_mapping_writer(map).unwrap();

        // Copy staging buffer to vertex buffer.
        // TODO: handle unified graphics/transfer queue differently.
        let submit = {
            let mut cbuf = graphics.transfer_command_pool.acquire_command_buffer(false);

            cbuf.copy_buffer(
                &staging_buffer,
                &vertex_buffer,
                &[BufferCopy {
                    src: 0,
                    dst: 0,
                    size: size,
                }],
            );

            let barrier = Barrier::Buffer {
                states: Access::TRANSFER_WRITE..Access::empty(),
                target: &vertex_buffer,
            };
            cbuf.pipeline_barrier(
                PipelineStage::TRANSFER..PipelineStage::BOTTOM_OF_PIPE,
                Dependencies::empty(),
                &[barrier],
            );

            cbuf.finish()
        };

        graphics
            .device
            .reset_fence(&graphics.transfer_fence)
            .unwrap();
        let submission = Submission::new().submit(Some(submit));
        graphics
            .queue_groups
            .transfer_queue()
            .submit(submission, Some(&graphics.transfer_fence));

        // Load shaders.
        let vs_module = {
            let spirv = include_bytes!(concat!(env!("OUT_DIR"), "/shaders/circle.vert.spirv"));
            graphics.device.create_shader_module(spirv).unwrap()
        };
        let fs_module = {
            let spirv = include_bytes!(concat!(env!("OUT_DIR"), "/shaders/circle.frag.spirv"));
            graphics.device.create_shader_module(spirv).unwrap()
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
        let descriptor_set_layout = graphics
            .device
            .create_descriptor_set_layout(&[global_ubo_layout_binding], &[])
            .unwrap();
        let global_ubo_descriptor_set = graphics
            .descriptor_pool
            .allocate_set(&descriptor_set_layout)
            .unwrap();
        let write = DescriptorSetWrite {
            set: &global_ubo_descriptor_set,
            binding: 0,
            array_offset: 0,
            descriptors: &[Descriptor::Buffer(
                &graphics.global_ubo,
                Some(0)..Some(GLOBAL_UBO_SIZE),
            )],
        };
        graphics.device.write_descriptor_sets(Some(write));

        // Create pipeline for circle rendering.
        let pipeline_layout = graphics
            .device
            .create_pipeline_layout(
                Some(&descriptor_set_layout),
                &[(ShaderStageFlags::GRAPHICS, 0..8)],
            )
            .unwrap();
        let pipeline = create_pipeline::<B>(
            &graphics.device,
            &vs_module,
            &fs_module,
            &pipeline_layout,
            &graphics.render_pass,
            graphics.swapchain_state.viewport.clone(),
        );

        // When transfer is finished, delete the staging buffers.
        // TODO: possibly need a pipeline barrier while drawing?
        // not sure it really matters?
        graphics
            .device
            .wait_for_fence(&graphics.transfer_fence, !0)
            .unwrap();
        graphics.device.destroy_buffer(staging_buffer);
        graphics.device.free_memory(staging_memory);

        CircleRenderer {
            vertex_buffer,
            vbo_memory,
            pipeline_layout,
            descriptor_set_layout,
            global_ubo_descriptor_set,
            vs_module,
            fs_module,
            pipeline,
        }
    }

    pub fn draw(&mut self, ctx: &mut DrawContext<B>, circles: &[Circle]) {
        if ctx.update_viewport {
            let pipeline = create_pipeline::<B>(
                ctx.device,
                &self.vs_module,
                &self.fs_module,
                &self.pipeline_layout,
                ctx.render_pass,
                ctx.viewport.clone(),
            );
            let pipeline = mem::replace(&mut self.pipeline, pipeline);
            ctx.device.destroy_graphics_pipeline(pipeline);
        }

        // TODO: re-use command buffers
        ctx.encoder
            .bind_vertex_buffers(0, [(&self.vertex_buffer, 0)].into_iter().cloned());
        ctx.encoder.bind_graphics_pipeline(&self.pipeline);
        ctx.encoder.bind_graphics_descriptor_sets(
            &self.pipeline_layout,
            0,
            Some(&self.global_ubo_descriptor_set),
            None as Option<u32>,
        );
        for circle in circles.iter() {
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
            let push_constants: [u32; 8] = unsafe { mem::transmute(push_constants) };
            ctx.encoder.push_graphics_constants(
                &self.pipeline_layout,
                ShaderStageFlags::GRAPHICS,
                0,
                &push_constants,
            );
            ctx.encoder.draw(0..4, 0..1);
        }
    }

    pub fn destroy(self, graphics: &mut Graphics<B>) {
        graphics.device.wait_idle().unwrap();
        graphics.device.destroy_buffer(self.vertex_buffer);
        graphics.device.free_memory(self.vbo_memory);
        graphics
            .device
            .destroy_pipeline_layout(self.pipeline_layout);
        graphics.device.destroy_graphics_pipeline(self.pipeline);
        graphics.device.destroy_shader_module(self.vs_module);
        graphics.device.destroy_shader_module(self.fs_module);
        graphics
            .device
            .destroy_descriptor_set_layout(self.descriptor_set_layout);
    }
}
