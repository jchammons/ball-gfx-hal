use crate::debug::DebugState;
use crate::game::{
    clamp_cursor,
    client::Game,
    GameSettings,
    GetPlayer,
    RoundState,
};
use crate::graphics::{Circle, CircleRenderer, DrawContext};
use crate::networking::{
    self,
    client::{self, ClientHandle, ConnectedHandle, ConnectingHandle},
    server::{self, ServerHandle},
};
use easer::functions::*;
use gfx_hal::Backend;
use imgui::{im_str, ImString, Ui};
use log::{error, warn};
use nalgebra::Point2;
use palette::LinSrgb;
use std::iter;
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Instant;
use winit::{
    dpi::LogicalSize,
    ElementState,
    MouseButton,
    VirtualKeyCode,
    WindowEvent,
};

const SCALE: f32 = 0.9;

fn bounds_circle(scale: f32, settings: Option<&GameSettings>) -> Circle {
    let bounds_radius = match settings {
        Some(settings) => settings.bounds_radius,
        None => 1.0,
    };
    Circle {
        center: Point2::new(0.0, 0.0),
        radius: scale * bounds_radius,
        color: LinSrgb::new(1.0, 1.0, 1.0),
    }
}

struct Connecting {
    server: Option<ServerHandle>,
    client: ClientHandle,
    done: ConnectingHandle,
}

pub struct GameState {
    error_text: Option<ImString>,
    server_addr: ImString,
    server_addr_host: ImString,
    cursor: Point2<f32>,
    screen: Screen,
}

enum Screen {
    MainMenu {
        connecting: Option<Connecting>,
    },
    InGame {
        server: Option<ServerHandle>,
        _client: ClientHandle,
        done: ConnectedHandle,
        game: Game,
        locked: bool,
        show_settings: bool,
    },
}

impl Connecting {
    fn host(
        addr: SocketAddr,
        debug: &DebugState,
        cursor: Point2<f32>,
    ) -> Result<Connecting, networking::Error> {
        let (server, _) = server::host(addr)?;
        let (client, done, _) =
            client::connect(addr, Some(debug.network_tx.clone()), cursor)?;
        Ok(Connecting {
            server: Some(server),
            client,
            done,
        })
    }

    fn connect(
        addr: SocketAddr,
        debug: &DebugState,
        cursor: Point2<f32>,
    ) -> Result<Connecting, networking::Error> {
        let (client, done, _) =
            client::connect(addr, Some(debug.network_tx.clone()), cursor)?;
        Ok(Connecting {
            server: None,
            client,
            done,
        })
    }
}

impl Default for GameState {
    fn default() -> GameState {
        GameState {
            error_text: None,
            server_addr: ImString::with_capacity(64),
            server_addr_host: ImString::new("0.0.0.0:6666"),
            cursor: Point2::new(0.0, 0.0),
            screen: Screen::MainMenu {
                connecting: None,
            },
        }
    }
}

impl GameState {
    pub fn handle_event(&mut self, size: &LogicalSize, event: &WindowEvent) {
        if let WindowEvent::CursorMoved {
            position,
            ..
        } = event
        {
            let scale = (2.0 / size.width.min(size.height) as f32) / SCALE;
            self.cursor = Point2::new(
                scale * (position.x as f32 - 0.5 * size.width as f32),
                scale * (position.y as f32 - 0.5 * size.height as f32),
            );
        }

        match self.screen {
            Screen::MainMenu {
                ..
            } => (),
            Screen::InGame {
                ref game,
                ref mut locked,
                ref mut show_settings,
                ..
            } => {
                match event {
                    WindowEvent::KeyboardInput {
                        input,
                        ..
                    } => {
                        match input.virtual_keycode {
                            Some(VirtualKeyCode::S)
                                if input.state == ElementState::Pressed =>
                            {
                                *show_settings = !*show_settings;
                            }
                            _ => (),
                        }
                    },
                    WindowEvent::CursorMoved {
                        ..
                    } if !*locked => game.update_cursor(self.cursor),
                    WindowEvent::MouseInput {
                        state: ElementState::Pressed,
                        button: MouseButton::Middle,
                        ..
                    } => {
                        *locked = !*locked;
                    },
                    _ => (),
                }
            },
        }
    }

    pub fn update(&mut self, dt: f32) {
        let error_text = &mut self.error_text;
        let transition = match self.screen {
            Screen::MainMenu {
                connecting: ref mut connecting_persist,
            } => {
                connecting_persist.take().and_then(|connecting| {
                    match connecting.done.try_recv() {
                        Ok(Ok((game, done))) => {
                            Some(Screen::InGame {
                                server: connecting.server,
                                _client: connecting.client,
                                done,
                                game,
                                locked: false,
                                show_settings: false,
                            })
                        },
                        Ok(Err(err)) => {
                            if let Some(err) = err {
                                let err = format!(
                                    "client connection failed: {}",
                                    err
                                );
                                error!("{}", err);
                                *error_text = Some(ImString::new(err));
                            }
                            None
                        },
                        Err(_) => {
                            *connecting_persist = Some(connecting);
                            None
                        },
                    }
                })
            },
            Screen::InGame {
                ref mut game,
                ref mut done,
                ref mut server,
                ..
            } => {
                game.tick(dt);
                // Check if either the server or client has shut down.
                server
                    .as_mut()
                    .and_then(|server| {
                        server.done.try_recv().ok().map(|err| {
                            if let Some(err) = err {
                                let err = format!(
                                    "server stopped with error: {}",
                                    err
                                );
                                error!("{}", err);
                                *error_text = Some(ImString::new(err));
                            }
                        })
                    })
                    .or_else(|| {
                        done.try_recv().ok().map(|err| {
                            if let Some(err) = err {
                                let err = format!(
                                    "client stopped with error: {}",
                                    err
                                );
                                error!("{}", err);
                                *error_text = Some(ImString::new(err));
                            }
                        })
                    })
                    .map(|_| {
                        Screen::MainMenu {
                            connecting: None,
                        }
                    })
            },
        };

        if let Some(screen) = transition {
            self.screen = screen;
        }
    }

    pub fn draw<B: Backend>(
        &mut self,
        now: Instant,
        circle_rend: &mut CircleRenderer<B>,
        ctx: &mut DrawContext<B>,
        debug: &DebugState,
    ) {
        match self.screen {
            Screen::MainMenu {
                ..
            } => {
                circle_rend.draw(ctx, iter::once(bounds_circle(SCALE, None)));
            },
            Screen::InGame {
                ref mut game,
                ..
            } => {
                // TODO use the z-buffer to reduce overdraw here

                game.clean_old_snapshots(now, debug.interpolation_delay);

                let (round_circles, scale) = match (game.last_round, game.round)
                {
                    (Some(RoundState::Winner(_)), RoundState::Waiting) => {
                        let scale = Expo::ease_out(
                            game.round_duration,
                            0.0,
                            SCALE,
                            0.3,
                        );
                        (None, scale)
                    },
                    (_, RoundState::Winner(winner)) => {
                        // No winner is black
                        let color = match winner {
                            Some(id) => game.players[&id].color,
                            None => LinSrgb::new(0.5, 0.5, 0.5),
                        };
                        let radius = Expo::ease_out(
                            game.round_duration,
                            0.0,
                            game.settings().bounds_radius,
                            0.3,
                        );

                        let scale = if game.round_duration > 0.5 {
                            Expo::ease_in(
                                game.round_duration - 0.5,
                                SCALE,
                                -SCALE,
                                0.3,
                            )
                            .max(0.0)
                        } else {
                            SCALE
                        };

                        (
                            Some(Circle {
                                center: Point2::new(0.0, 0.0),
                                radius: scale * radius,
                                color,
                            }),
                            scale,
                        )
                    },
                    _ => (None, SCALE),
                };

                let players = game.interpolated_players(
                    now,
                    clamp_cursor(self.cursor, game.settings()),
                    debug.interpolation_delay,
                );
                let circles = players.into_iter().flat_map(|(_, player)| {
                    player.draw(scale, game.settings())
                });

                let bounds_circle = bounds_circle(scale, Some(game.settings()));

                if debug.draw_latest_snapshot {
                    let players = game.latest_players();
                    let debug_circles = players
                        .into_iter()
                        .flat_map(|(_, player)| {
                            player.draw(scale, game.settings())
                        })
                        .map(|circle| {
                            Circle {
                                color: LinSrgb::new(0.8, 0.0, 0.0),
                                ..circle
                            }
                        });
                    circle_rend.draw(
                        ctx,
                        iter::once(bounds_circle)
                            .chain(debug_circles)
                            .chain(circles)
                            .chain(round_circles),
                    );
                } else {
                    circle_rend.draw(
                        ctx,
                        iter::once(bounds_circle)
                            .chain(circles)
                            .chain(round_circles),
                    );
                }
            },
        }
    }

    pub fn ui<'a>(&mut self, ui: &Ui<'a>, debug: &DebugState) {
        if let Some(ref err) = self.error_text {
            ui.open_popup(im_str!("error"));
            let mut open = true;
            ui.popup_modal(im_str!("error")).build(|| {
                ui.text_wrapped(err);
                if ui.small_button(im_str!("OK")) {
                    ui.close_current_popup();
                    open = false;
                }
                // This is to force the window size up to a certain
                // point. Blocked on:
                // https://github.com/Gekkio/imgui-rs/issues/201.
                ui.dummy((500.0, 0.0));
            });
            if !open {
                self.error_text = None;
            }
        }

        match self.screen {
            Screen::MainMenu {
                ref mut connecting,
            } => {
                let server_addr = &mut self.server_addr;
                let server_addr_host = &mut self.server_addr_host;
                let error_text = &mut self.error_text;
                let cursor = self.cursor;
                ui.window(im_str!("Main Menu")).always_auto_resize(true).build(
                    || {
                        if connecting.is_some() {
                            ui.text(im_str!("Connecting..."));
                            ui.same_line(0.0);
                            if ui.small_button(im_str!("Cancel")) {
                                *connecting = None;
                            }
                            ui.separator();
                        }

                        ui.input_text(im_str!("Remote address"), server_addr)
                            .build();
                        if ui.small_button(im_str!("Connect to server")) {
                            match server_addr.to_str().to_socket_addrs() {
                                Ok(mut addrs) => {
                                    match addrs.next() {
                                        Some(addr) => {
                                            match Connecting::connect(
                                                addr, debug, cursor,
                                            ) {
                                                Ok(state) => {
                                                    *connecting = Some(state)
                                                },
                                                Err(err) => {
                                                    let err = format!(
                                                        "error connecting to \
                                                         server: {}",
                                                        err
                                                    );
                                                    error!("{}", err);
                                                    *error_text = Some(
                                                        ImString::new(err),
                                                    );
                                                },
                                            }
                                        },
                                        None => {
                                            let err = format!(
                                                "couldn't resolve server \
                                                 address: {}",
                                                server_addr.to_str()
                                            );
                                            warn!("{}", err);
                                            *error_text =
                                                Some(ImString::new(err));
                                        },
                                    }
                                },
                                Err(_) => {
                                    let err = format!(
                                        "couldn't parse server address: {}",
                                        server_addr.to_str()
                                    );
                                    warn!("{}", err);
                                    *error_text = Some(ImString::new(err));
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
                            match server_addr_host.to_str().to_socket_addrs() {
                                Ok(mut addrs) => {
                                    match addrs.next() {
                                        Some(addr) => {
                                            match Connecting::host(
                                                addr, debug, cursor,
                                            ) {
                                                Ok(state) => {
                                                    *connecting = Some(state)
                                                },
                                                Err(err) => {
                                                    let err = format!(
                                                        "error hosting \
                                                         server: {}",
                                                        err
                                                    );
                                                    error!("{}", err);
                                                    *error_text = Some(
                                                        ImString::new(err),
                                                    );
                                                },
                                            }
                                        },
                                        None => {
                                            let err = format!(
                                                "Couldn't resolve server \
                                                 hosting address: {}",
                                                server_addr_host.to_str()
                                            );
                                            warn!("{}", err);
                                            *error_text =
                                                Some(ImString::new(err));
                                        },
                                    }
                                },
                                Err(_) => {
                                    let err = format!(
                                        "Couldn't parse server hosting \
                                         address: {}",
                                        server_addr_host.to_str()
                                    );
                                    warn!("{}", err);
                                    *error_text = Some(ImString::new(err));
                                },
                            }
                        }
                    },
                );
            },
            Screen::InGame {
                ref show_settings,
                ref mut game,
                ..
            } => {
                if *show_settings {
                    ui.window(im_str!("Game Settings"))
                        .always_auto_resize(true)
                        .build(|| {
                            let mut settings = *game.settings();
                            let mut changed = false;
                            changed |= ui
                                .input_float(
                                    im_str!("ball radius"),
                                    &mut settings.ball_radius,
                                )
                                .build();
                            changed |= ui
                                .input_float(
                                    im_str!("cursor radius"),
                                    &mut settings.cursor_radius,
                                )
                                .build();
                            changed |= ui
                                .input_float(
                                    im_str!("spring constant"),
                                    &mut settings.spring_constant,
                                )
                                .build();
                            changed |= ui
                                .input_float(
                                    im_str!("ball start distance"),
                                    &mut settings.ball_start_distance,
                                )
                                .build();
                            changed |= ui
                                .input_float(
                                    im_str!("ball start speed"),
                                    &mut settings.ball_start_speed,
                                )
                                .build();
                            changed |= ui
                                .input_float(
                                    im_str!("bounds radius"),
                                    &mut settings.bounds_radius,
                                )
                                .build();
                            if changed {
                                game.set_settings(settings);
                            }
                        });
                }
            },
        }
    }
}
