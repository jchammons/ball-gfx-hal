use crate::game::GameClient;
use crate::graphics::{Circle, CircleRenderer, DrawContext};
use crate::networking::{
    self,
    client::{self, ClientHandle, ConnectingHandle},
    server::{self, ServerHandle},
};
use cgmath::Point2;
use gfx_hal::Backend;
use imgui::{im_str, ImString, Ui};
use log::{error, warn};
use palette::LinSrgb;
use std::marker::PhantomData;
use std::mem;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use winit::{Window, WindowEvent};

const BOUNDS_CIRCLE: Circle = Circle {
    center: Point2 { x: 0.0, y: 0.0 },
    radius: 0.9,
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
            GameState::InGame { game, .. } => match event {
                WindowEvent::CursorMoved { position, .. } => {
                    let size = window.get_inner_size().unwrap();
                    let scale = 2.0 / size.width.min(size.height) as f32;
                    let position = Point2::new(
                        scale * (position.x as f32 - 0.5 * size.width as f32),
                        scale * (position.y as f32 - 0.5 * size.height as f32),
                    );
                    game.update_position(position);
                }
                _ => (),
            },
        }
    }

    pub fn update(&mut self, _dt: f32) {
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
                        });
                    }
                    Some((Err(err), _)) => {
                        error!("failed to connect: {}", err);
                        *connecting = None;
                    }
                    None => (),
                }
            }
            _ => (),
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
                        if let Some(_) = connecting {
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

    pub fn draw<'a, B: Backend>(
        &mut self,
        now: Instant,
        circle_rend: &mut CircleRenderer<B>,
        ctx: &mut DrawContext<B>,
    ) {
        match self {
            GameState::MainMenu { .. } => {
                let circles = [BOUNDS_CIRCLE];
                circle_rend.draw(ctx, &circles);
            }
            GameState::InGame { game, .. } => {
                let mut circles = Vec::new();
                circles.push(BOUNDS_CIRCLE);
                let players = game.players.lock();
                let snapshot = game.interpolate_snapshot(now);
                for (id, player) in snapshot.players() {
                    let player = Circle {
                        center: player.position,
                        radius: 0.1,
                        color: players[&id].color,
                    };
                    circles.push(player);
                }
                circle_rend.draw(ctx, &circles);
            }
        }
    }
}
