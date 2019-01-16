use crate::game::{client::Game, GetPlayer};
use crate::graphics::{Circle, CircleRenderer, DrawContext};
use crate::networking::{
    self,
    client::{self, ClientHandle, ConnectingHandle},
    server::{self, ServerHandle},
};
use gfx_hal::Backend;
use imgui::{im_str, ImString, Ui};
use log::{error, warn};
use nalgebra::Point2;
use palette::LinSrgb;
use std::iter;
use std::marker::PhantomData;
use std::mem;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use winit::{
    dpi::LogicalPosition,
    dpi::LogicalSize,
    ElementState,
    MouseButton,
    WindowEvent,
};

const SCALE: f32 = 0.9;
/// This is a function since `Point2::new` isn't `const fn`.
///
/// Hopefully the compiler can optimize this as expected.
fn bounds_circle() -> Circle {
    Circle {
        center: Point2::new(0.0, 0.0),
        radius: SCALE,
        color: LinSrgb {
            red: 1.0,
            green: 1.0,
            blue: 1.0,
            standard: PhantomData,
        },
    }
}

#[derive(Debug, Copy, Clone)]
pub struct DebugOptions {
    pub draw_latest_snapshot: bool,
    pub interpolation_delay: f32,
}

impl Default for DebugOptions {
    fn default() -> DebugOptions {
        DebugOptions {
            draw_latest_snapshot: false,
            interpolation_delay: 1.5,
        }
    }
}

pub struct ConnectingState {
    server: Option<ServerHandle>,
    client: ClientHandle,
    connecting: ConnectingHandle,
}

pub enum GameState {
    MainMenu {
        server_addr: ImString,
        server_addr_host: ImString,
        connecting: Option<ConnectingState>,
        cursor: Point2<f32>,
    },
    InGame {
        server: Option<ServerHandle>,
        client: ClientHandle,
        game: Arc<Game>,
        locked: bool,
    },
}

impl ConnectingState {
    fn host(
        addr: SocketAddr,
        cursor: Point2<f32>,
    ) -> Result<ConnectingState, networking::Error> {
        let (server, _) = server::host(addr)?;
        let (client, connecting) = client::connect(addr, cursor)?;
        Ok(ConnectingState {
            server: Some(server),
            client,
            connecting,
        })
    }

    fn connect(
        addr: SocketAddr,
        cursor: Point2<f32>,
    ) -> Result<ConnectingState, networking::Error> {
        let (client, connecting) = client::connect(addr, cursor)?;
        Ok(ConnectingState {
            server: None,
            client,
            connecting,
        })
    }
}

impl Default for GameState {
    fn default() -> GameState {
        GameState::MainMenu {
            server_addr: ImString::with_capacity(64),
            server_addr_host: ImString::new("0.0.0.0:6666"),
            connecting: None,
            cursor: Point2::new(0.0, 0.0),
        }
    }
}

impl GameState {
    pub fn transition_to(&mut self, state: GameState) {
        mem::replace(self, state);
    }

    pub fn handle_event(&mut self, size: &LogicalSize, event: &WindowEvent) {
        let cursor_pos = |position: &LogicalPosition| {
            let scale = (2.0 / size.width.min(size.height) as f32) / SCALE;
            Point2::new(
                scale * (position.x as f32 - 0.5 * size.width as f32),
                scale * (position.y as f32 - 0.5 * size.height as f32),
            )
        };

        match self {
            GameState::MainMenu {
                ref mut cursor,
                ..
            } => {
                match event {
                    WindowEvent::CursorMoved {
                        position,
                        ..
                    } => {
                        *cursor = cursor_pos(position);
                    },
                    _ => (),
                }
            },
            GameState::InGame {
                ref game,
                ref mut locked,
                ..
            } => {
                match event {
                    WindowEvent::CursorMoved {
                        position,
                        ..
                    } if !*locked => {
                        game.update_cursor(cursor_pos(position));
                    },
                    WindowEvent::MouseInput {
                        state,
                        button: MouseButton::Left,
                        ..
                    } => {
                        *locked = *state == ElementState::Pressed;
                    },
                    WindowEvent::Focused(true) => {
                        *locked = false;
                    },
                    _ => (),
                }
            },
        }
    }

    pub fn update(&mut self, dt: f32) {
        let mut transition = None;

        match self {
            GameState::MainMenu {
                ref mut connecting,
                ..
            } => {
                let done = connecting
                    .as_mut()
                    .and_then(|state| state.connecting.done())
                    .map(|done| (done, connecting.take().unwrap()));
                match done {
                    Some((Ok(game), connecting)) => {
                        transition = Some(GameState::InGame {
                            server: connecting.server,
                            client: connecting.client,
                            game,
                            locked: false,
                        });
                    },
                    Some((Err(err), _)) => {
                        error!("failed to connect: {}", err);
                        *connecting = None;
                    },
                    None => (),
                }
            },
            GameState::InGame {
                ref game,
                ..
            } => {
                game.tick(dt);
            },
        };

        if let Some(transition) = transition {
            self.transition_to(transition);
        }
    }

    pub fn ui<'a>(&mut self, ui: &Ui<'a>) {
        match self {
            GameState::MainMenu {
                ref mut server_addr,
                ref mut server_addr_host,
                ref mut connecting,
                ref cursor,
            } => {
                ui.window(im_str!("Main Menu")).always_auto_resize(true).build(
                    || {
                        if connecting.is_some() {
                            ui.text(im_str!("Connecting..."));
                            ui.separator();
                        }

                        ui.input_text(im_str!("Remote address"), server_addr)
                            .build();
                        if ui.small_button(im_str!("Connect to server")) {
                            match server_addr.to_str().parse() {
                                Ok(addr) => {
                                    match ConnectingState::connect(
                                        addr, *cursor,
                                    ) {
                                        Ok(state) => *connecting = Some(state),
                                        Err(err) => {
                                            error!(
                                                "error hosting server: {}",
                                                err
                                            )
                                        },
                                    }
                                },
                                Err(_) => {
                                    warn!(
                                        "Couldn't parse server address: {}",
                                        server_addr.to_str()
                                    )
                                },
                            }
                        }

                        ui.separator();

                        ui.input_text(
                            im_str!("Host address"),
                            server_addr_host,
                        )
                        .build();
                        if ui.small_button(im_str!("Host server")) {
                            match server_addr_host.to_str().parse() {
                                Ok(addr) => {
                                    match ConnectingState::host(addr, *cursor) {
                                        Ok(state) => *connecting = Some(state),
                                        Err(err) => {
                                            error!(
                                                "error connecting to server: \
                                                 {}",
                                                err
                                            )
                                        },
                                    }
                                },
                                Err(_) => {
                                    warn!(
                                        "Couldn't parse server hosting \
                                         address: {}",
                                        server_addr_host.to_str()
                                    )
                                },
                            }
                        }
                    },
                );
            },
            GameState::InGame {
                ..
            } => (),
        }
    }

    pub fn draw<B: Backend>(
        &mut self,
        now: Instant,
        circle_rend: &mut CircleRenderer<B>,
        ctx: &mut DrawContext<B>,
        debug: DebugOptions,
    ) {
        match self {
            GameState::MainMenu {
                ..
            } => {
                circle_rend.draw(ctx, iter::once(bounds_circle()));
            },
            GameState::InGame {
                game,
                ..
            } => {
                // TODO use the z-buffer to reduce overdraw here
                circle_rend.draw(ctx, iter::once(bounds_circle()));
                if debug.draw_latest_snapshot {
                    // TODO avoid submitting a second drawcall here
                    game.latest_players(|players| {
                        let circles = players
                            .into_iter()
                            .flat_map(|(_, player)| player.draw(SCALE))
                            .map(|circle| {
                                Circle {
                                    color: LinSrgb::new(0.8, 0.0, 0.0),
                                    ..circle
                                }
                            });
                        circle_rend.draw(ctx, circles);
                    });
                }
                game.interpolated_players(
                    now,
                    debug.interpolation_delay,
                    |players| {
                        let circles = players
                            .into_iter()
                            .flat_map(|(_, player)| player.draw(SCALE));
                        circle_rend.draw(ctx, circles);
                    },
                );
            },
        }
    }
}
