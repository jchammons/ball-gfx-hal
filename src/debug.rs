use crate::graphics::{self, renderdoc::RenderDoc, Graphics};
use crate::logger;
use crate::ui;
use gfx_hal::{Backend, PresentMode};
use imgui::{im_str, ImString, Ui};

const FRAME_TIME_HISTORY_LENGTH: usize = 256;

/// State and options related to the debug window.
#[derive(Clone)]
pub struct DebugState {
    /// Whether to draw an overlay showing the most recently received
    /// snapshot.
    ///
    /// This is useful to debug the difference between the
    /// interpolated visual positions and the raw snapshots.
    pub draw_latest_snapshot: bool,
    /// The delay in multiples of the snapshot rate to buffer
    /// snapshots for interpolation.
    ///
    /// Increasing this will make things smoother in the presence of
    /// packet loss or jitter, but will increase visual latency.
    pub interpolation_delay: f32,
    frame_time_history: [f32; FRAME_TIME_HISTORY_LENGTH],
}

impl Default for DebugState {
    fn default() -> DebugState {
        DebugState {
            draw_latest_snapshot: false,
            interpolation_delay: 1.5,
            frame_time_history: [0.0; FRAME_TIME_HISTORY_LENGTH],
        }
    }
}

impl DebugState {
    /// Draws the debug window into imgui.
    pub fn ui<'a, B: Backend>(
        &mut self,
        ui: &Ui<'a>,
        graphics: &mut Graphics<B>,
        renderdoc: &mut RenderDoc,
        frame_time: f32,
    ) {
        // Convert frame_time to ms.
        let frame_time = frame_time * 1000.0;

        // Log the frame time.
        for i in 0..FRAME_TIME_HISTORY_LENGTH - 1 {
            self.frame_time_history[i] = self.frame_time_history[i + 1];
        }
        self.frame_time_history[FRAME_TIME_HISTORY_LENGTH - 1] = frame_time;

        ui.window(im_str!("Debug")).build(|| {
            ui.tree_node(im_str!("Networking")).build(|| {
                ui.checkbox(
                    im_str!("Draw latest snapshot"),
                    &mut self.draw_latest_snapshot,
                );

                ui.input_float(
                    im_str!("Interpolation delay"),
                    &mut self.interpolation_delay,
                )
                .build();
            });

            ui.tree_node(im_str!("Graphics")).build(|| {
                ui.plot_lines(im_str!("Frame time"), &self.frame_time_history)
                    .scale_max(1000.0 / 20.0)
                    .scale_min(0.0)
                    .overlay_text(&ImString::new(format!(
                        "{:.2} ms",
                        frame_time
                    )))
                    .build();

                let mut present_mode = graphics.present_mode();
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
                    graphics::renderdoc::trigger_capture(renderdoc, 1);
                }
            });

            ui.tree_node(im_str!("Logger")).build(|| {
                logger::LOGGER.ui(&ui);
            });
        });
    }
}
