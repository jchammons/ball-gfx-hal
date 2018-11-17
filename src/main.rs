extern crate gfx_backend_vulkan as backend;
use env_logger;
use imgui::{im_str, ImGui, ImString};
use imgui_winit::ImGuiWinit;
use winit::{Event, EventsLoop, Window, WindowEvent};

pub mod graphics;

fn main() {
    env_logger::init();

    let mut imgui = ImGui::init();
    let mut imgui_winit = ImGuiWinit::new(&mut imgui);
    let mut events_loop = EventsLoop::new();
    let window = Window::new(&events_loop).unwrap();

    let instance = backend::Instance::create("Ball", 1);
    let surface = instance.create_surface(&window);
    let mut graphics = graphics::Graphics::new(instance, surface, &mut imgui);

    let mut renderdoc = graphics::renderdoc::init();

    let mut running = true;
    let mut server_addr = ImString::with_capacity(128);
    while running {
        events_loop.poll_events(|event| {
            imgui_winit.handle_event(&mut imgui, &event);

            if let Event::WindowEvent { event, .. } = event {
                match event {
                    WindowEvent::CloseRequested => running = false,
                    WindowEvent::Resized(_) => {
                        graphics::renderdoc::trigger_capture(&mut renderdoc, 1);
                        graphics.resize()
                    }
                    _ => (),
                }
            }
        });

        let ui = imgui_winit.frame(&mut imgui, &window);
        ui.window(im_str!("Ball")).build(|| {
            ui.input_text(im_str!("Server Address"), &mut server_addr)
                .build();
            ui.small_button(im_str!("Connect"));
        });
        if let Err(_) = graphics.draw_frame(ui) {
            // Ignore it for now?
        }
    }

    graphics.destroy();
}
