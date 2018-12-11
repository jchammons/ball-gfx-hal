use crate::game::{GameClient, PlayerClient, BALL_RADIUS, CURSOR_RADIUS};
use crate::graphics::{Circle, CircleRenderer, DrawContext};
use crate::networking::{
    self,
    client::{self, ClientHandle, ConnectingHandle},
    server::{self, ServerHandle},
};
use arrayvec::ArrayVec;
use cgmath::Point2;
use gfx_hal::Backend;
use imgui::{im_str, ImString, Ui};
use log::{error, warn};
use palette::LinSrgb;
use std::iter;
use std::marker::PhantomData;
use std::mem;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use winit::{ElementState, MouseButton, Window, WindowEvent};

const SCALE: f32 = 0.9;
const BOUNDS_CIRCLE: Circle = Circle {
    center: Point2 { x: 0.0, y: 0.0 },
    radius: SCALE,
    color: LinSrgb {
        red: 1.0,
        green: 1.0,
        blue: 1.0,
        standard: PhantomData,
    },
};

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
    },
    InGame {
        server: Option<ServerHandle>,
        client: ClientHandle,
        game: Arc<GameClient>,
        locked: bool,
    },
}

impl ConnectingState {
    fn host(addr: SocketAddr) -> Result<ConnectingState, networking::Error> {
        let (server, _) = server::host(addr)?;
        let (client, connecting) = client::connect(addr)?;
        Ok(ConnectingState {
            server: Some(server),
            client,
            connecting,
        })
    }

    fn connect(addr: SocketAddr) -> Result<ConnectingState, networking::Error> {
        let (client, connecting) = client::connect(addr)?;
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
        }
    }
}

impl GameState {
    pub fn transition_to(&mut self, state: GameState) {
        mem::replace(self, state);
    }

    pub fn handle_event(&mut self, window: &Window, event: &WindowEvent) {
        match self {
            GameState::MainMenu { .. } => (),
            GameState::InGame {
                ref game,
                ref mut locked,
                ..
            } => match event {
                WindowEvent::CursorMoved { position, .. } if !*locked => {
                    let size = window.get_inner_size().unwrap();
                    let scale = (2.0 / size.width.min(size.height) as f32) / SCALE;
                    let position = Point2::new(
                        scale * (position.x as f32 - 0.5 * size.width as f32),
                        scale * (position.y as f32 - 0.5 * size.height as f32),
                    );
                    game.update_position(position);
                }
                WindowEvent::MouseInput {
                    state,
                    button: MouseButton::Left,
                    ..
                } => {
                    *locked = *state == ElementState::Pressed;
                }
                WindowEvent::Focused(true) => {
                    *locked = false;
                }
                _ => (),
            },
        }
    }

    pub fn update(&mut self, dt: f32) {
        let mut transition = None;

        match self {
            GameState::MainMenu {
                ref mut connecting, ..
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
                    }
                    Some((Err(err), _)) => {
                        error!("failed to connect: {}", err);
                        *connecting = None;
                    }
                    None => (),
                }
            }
            GameState::InGame { ref game, .. } => {
                let mut dt = dt;
                while dt > 1.0 / 60.0 {
                    game.tick(1.0 / 60.0);
                    dt -= 1.0 / 60.0;
                }
                game.tick(dt);
            }
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
            } => {
                ui.window(im_str!("Main Menu"))
                    .always_auto_resize(true)
                    .build(|| {
                        if connecting.is_some() {
                            ui.text(im_str!("Connecting..."));
                            ui.separator();
                        }

                        ui.input_text(im_str!("Remote address"), server_addr)
                            .build();
                        if ui.small_button(im_str!("Connect to server")) {
                            match server_addr.to_str().parse() {
                                Ok(addr) => match ConnectingState::connect(addr) {
                                    Ok(state) => *connecting = Some(state),
                                    Err(err) => error!("error hosting server: {}", err),
                                },
                                Err(_) => {
                                    warn!("Couldn't parse server address: {}", server_addr.to_str())
                                }
                            }
                        }

                        ui.separator();

                        ui.input_text(im_str!("Host address"), server_addr_host)
                            .build();
                        if ui.small_button(im_str!("Host server")) {
                            match server_addr_host.to_str().parse() {
                                Ok(addr) => match ConnectingState::host(addr) {
                                    Ok(state) => *connecting = Some(state),
                                    Err(err) => error!("error connecting to server: {}", err),
                                },
                                Err(_) => warn!(
                                    "Couldn't parse server hosting address: {}",
                                    server_addr_host.to_str()
                                ),
                            }
                        }
                    });
            }
            GameState::InGame { .. } => (),
        }
    }

    pub fn draw<B: Backend>(
        &mut self,
        now: Instant,
        circle_rend: &mut CircleRenderer<B>,
        ctx: &mut DrawContext<B>,
    ) {
        match self {
            GameState::MainMenu { .. } => {
                circle_rend.draw(ctx, iter::once(BOUNDS_CIRCLE));
            }
            GameState::InGame { game, .. } => {
                let players = game.players.lock();
                let snapshot = game.interpolate_snapshot(now);
                let circles = iter::once(BOUNDS_CIRCLE).chain(
                    snapshot
                        .players()
                        .filter_map(|(id, player)| {
                            players.get(&id).map(|&PlayerClient { color, .. }| {
                                let cursor = Circle {
                                    center: player.position * SCALE,
                                    radius: CURSOR_RADIUS * SCALE,
                                    color,
                                };
                                let ball = Circle {
                                    center: player.position_ball * SCALE,
                                    radius: BALL_RADIUS * SCALE,
                                    color,
                                };
                                ArrayVec::from([cursor, ball])
                            })
                        })
                        .flatten(),
                );

                circle_rend.draw(ctx, circles);
            }
        }
    }
}
