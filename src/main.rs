extern crate gfx_backend_vulkan as backend;
use cgmath::Point2;
use env_logger;
use imgui::{im_str, ImGui, ImString};
use imgui_winit::ImGuiWinit;
use palette::LinSrgb;
use rand::{thread_rng, Rng};
use std::time::Instant;
use winit::{Event, EventsLoop, Window, WindowEvent};

pub mod graphics;

const FRAME_TIME_HISTORY_LENGTH: usize = 200;

fn main() {
    let mut rng = thread_rng();

    env_logger::init();

    let mut vsync = true;
    let mut num_circles = 100;

    let mut frame_time_history = Vec::with_capacity(FRAME_TIME_HISTORY_LENGTH);
    frame_time_history.resize(FRAME_TIME_HISTORY_LENGTH, 0.0);

    let mut imgui = ImGui::init();
    let mut imgui_winit = ImGuiWinit::new(&mut imgui);
    let mut events_loop = EventsLoop::new();
    let window = Window::new(&events_loop).unwrap();

    let instance = backend::Instance::create("Ball", 1);
    let surface = instance.create_surface(&window);
    let mut graphics = graphics::Graphics::new(instance, surface, &mut imgui, vsync);
    let mut circle_rend = graphics::CircleRenderer::new(&mut graphics);
    let circles: Vec<_> = (0..500000)
        .map(|_| graphics::Circle {
            center: Point2::new(rng.gen_range(-1.0, 1.0), rng.gen_range(-1.0, 1.0)),
            radius: rng.gen_range(0.02, 0.3),
            color: LinSrgb::new(
                rng.gen_range(0.0, 1.0),
                rng.gen_range(0.0, 1.0),
                rng.gen_range(0.0, 1.0),
            )
            .into_encoding(),
        })
        .collect();

    let mut renderdoc = graphics::renderdoc::init();

    let mut running = true;
    let mut server_addr = ImString::with_capacity(128);
    let mut last_frame = Instant::now();
    while running {
        events_loop.poll_events(|event| {
            imgui_winit.handle_event(&mut imgui, &event);

            if let Event::WindowEvent { event, .. } = event {
                match event {
                    WindowEvent::CloseRequested => running = false,
                    _ => (),
                }
            }
        });

        let now = Instant::now();
        let frame_time = now.duration_since(last_frame);
        let frame_time =
            frame_time.as_secs() as f32 + frame_time.subsec_nanos() as f32 / 1_000_000_000.0;
        last_frame = now;
        // Shift the fps history buffer.
        for i in 0..FRAME_TIME_HISTORY_LENGTH - 1 {
            frame_time_history[i] = frame_time_history[i + 1];
        }
        *frame_time_history.last_mut().unwrap() = frame_time;

        let ui = imgui_winit.frame(&mut imgui, &window);
        ui.window(im_str!("Ball")).build(|| {
            ui.input_text(im_str!("Server Address"), &mut server_addr)
                .build();
            ui.small_button(im_str!("Connect"));
        });
        ui.window(im_str!("Debug")).build(|| {
            ui.plot_lines(im_str!("Frame time"), &frame_time_history)
                .scale_max(1.0 / 20.0)
                .scale_min(0.0)
                .overlay_text(&ImString::new(format!("{:.2} ms", frame_time * 1000.0)))
                .build();
            ui.slider_int(im_str!("Circles"), &mut num_circles, 100, 500000)
                .build();
            if ui.checkbox(im_str!("Vsync"), &mut vsync) {
                graphics.set_vsync(vsync);
            }
        });
        if let Err(_) = graphics.draw_frame(ui, |mut ctx| {
            circle_rend.draw(&mut ctx, &circles[..num_circles as usize]);
        }) {
            graphics::renderdoc::trigger_capture(&mut renderdoc, 1);
            // Ignore it for now?
        }
    }

    circle_rend.destroy(&mut graphics);
    graphics.destroy();
}
