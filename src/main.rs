#![feature(duration_float, range_contains)]

extern crate gfx_backend_vulkan as backend;
use ctrlc;
use gfx_hal::PresentMode;
use imgui::{im_str, ImGui, ImString};
use imgui_winit::ImGuiWinit;
use std::net::SocketAddr;
use std::time::Instant;
use structopt::StructOpt;
use winit::{Event, EventsLoop, Window, WindowEvent};

pub mod double_buffer;
pub mod game;
pub mod graphics;
pub mod logger;
pub mod networking;
pub mod state;
pub mod ui;

const FRAME_TIME_HISTORY_LENGTH: usize = 200;

#[derive(StructOpt, Debug)]
#[structopt(name = "ball-gfx-hal")]
struct Cli {
    /// Instead of opening a gui window, host a headless server on
    /// this address.
    #[structopt(short = "s", long = "server")]
    host_server: Option<SocketAddr>,
}

fn main() {
    logger::apply().unwrap();

    let cli = Cli::from_args();

    match cli.host_server {
        Some(addr) => {
            let (server, thread) = networking::server::host(addr).unwrap();
            ctrlc::set_handler(move || {
                server.shutdown();
            })
            .unwrap();
            thread.join().unwrap();
        }
        None => run_gui(),
    }
}

fn run_gui() {
    let mut present_mode = PresentMode::Immediate;

    let mut frame_time_history = Vec::with_capacity(FRAME_TIME_HISTORY_LENGTH);
    frame_time_history.resize(FRAME_TIME_HISTORY_LENGTH, 0.0);

    let mut imgui = ImGui::init();
    let mut imgui_winit = ImGuiWinit::new(&mut imgui);
    let mut events_loop = EventsLoop::new();
    let window = Window::new(&events_loop).unwrap();

    let instance = backend::Instance::create("Ball", 1);
    let surface = instance.create_surface(&window);
    let mut graphics = graphics::Graphics::new(&instance, surface, &mut imgui, present_mode);
    let mut circle_rend = graphics::CircleRenderer::new(&mut graphics);

    let mut renderdoc = graphics::renderdoc::init();

    let mut game_state = state::GameState::default();

    let mut running = true;
    let mut last_frame = Instant::now();
    while running {
        // Wait for vertical blank/etc. before even starting to render.
        graphics.wait_for_frame();

        events_loop.poll_events(|event| {
            imgui_winit.handle_event(&mut imgui, &event);

            if let Event::WindowEvent { event, .. } = event {
                game_state.handle_event(&window, &event);
                if let WindowEvent::CloseRequested = event {
                    running = false;
                }
            }
        });

        let now = Instant::now();
        let frame_time = now.duration_since(last_frame).as_float_secs() as f32;
        last_frame = now;
        // Shift the fps history buffer.
        for i in 0..FRAME_TIME_HISTORY_LENGTH - 1 {
            frame_time_history[i] = frame_time_history[i + 1];
        }
        *frame_time_history.last_mut().unwrap() = frame_time;

        game_state.update(frame_time);

        let ui = imgui_winit.frame(&mut imgui, &window);
        ui.window(im_str!("Debug")).build(|| {
            ui.tree_node(im_str!("Graphics")).build(|| {
                ui.plot_lines(im_str!("Frame time"), &frame_time_history)
                    .scale_max(1.0 / 20.0)
                    .scale_min(0.0)
                    .overlay_text(&ImString::new(format!("{:.2} ms", frame_time * 1000.0)))
                    .build();

                if ui::enum_combo(
                    &ui,
                    im_str!("Present mode"),
                    &mut present_mode,
                    &[
                        im_str!("immediate"),
                        im_str!("relaxed"),
                        im_str!("fifo"),
                        im_str!("mailbox"),
                    ],
                    &[
                        PresentMode::Immediate,
                        PresentMode::Relaxed,
                        PresentMode::Fifo,
                        PresentMode::Mailbox,
                    ],
                    4,
                ) {
                    graphics.set_present_mode(present_mode);
                }

                if ui.small_button(im_str!("Capture frame")) {
                    graphics::renderdoc::trigger_capture(&mut renderdoc, 1);
                }
            });

            ui.tree_node(im_str!("Logger")).build(|| {
                logger::LOGGER.ui(&ui);
            });
        });
        game_state.ui(&ui);

        let _ = graphics.draw_frame(ui, |mut ctx| {
            game_state.draw(now, &mut circle_rend, &mut ctx);
        });
    }

    circle_rend.destroy(&mut graphics);
    graphics.destroy();
}
