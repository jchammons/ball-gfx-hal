use crate::debug::DebugState;
use crate::game::{clamp_cursor, client::Game, GetPlayer};
use crate::graphics::{Circle, CircleRenderer, DrawContext};
use crate::networking::{
    self,
    client::{self, ClientHandle, ConnectedHandle, ConnectingHandle},
    server::{self, ServerHandle},
};
use gfx_hal::Backend;
use imgui::{im_str, ImString, Ui};
use log::{error, warn};
use nalgebra::Point2;
use palette::LinSrgb;
use std::iter;
use std::net::SocketAddr;
use std::time::Instant;
use winit::{dpi::LogicalSize, ElementState, MouseButton, WindowEvent};

const SCALE: f32 = 0.9;
/// This is a function since `Point2::new` isn't `const fn`.
///
/// Hopefully the compiler can optimize this as expected.
fn bounds_circle() -> Circle {
    Circle {
        center: Point2::new(0.0, 0.0),
        radius: SCALE,
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
                ..
            } => {
                match event {
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
                circle_rend.draw(ctx, iter::once(bounds_circle()));
            },
            Screen::InGame {
                ref mut game,
                ..
            } => {
                // TODO use the z-buffer to reduce overdraw here
                circle_rend.draw(ctx, iter::once(bounds_circle()));

                if debug.draw_latest_snapshot {
                    // TODO avoid submitting a second drawcall here
                    let players = game.latest_players();
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
                }

                let players = game.interpolated_players(
                    now,
                    clamp_cursor(self.cursor),
                    debug.interpolation_delay,
                );
                let circles = players
                    .into_iter()
                    .flat_map(|(_, player)| player.draw(SCALE));

                circle_rend.draw(ctx, circles);
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
                            match server_addr.to_str().parse() {
                                Ok(addr) => {
                                    match Connecting::connect(
                                        addr, debug, cursor,
                                    ) {
                                        Ok(state) => *connecting = Some(state),
                                        Err(err) => {
                                            let err = format!(
                                                "error connecting to server: \
                                                 {}",
                                                err
                                            );
                                            error!("{}", err);
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
                            match server_addr_host.to_str().parse() {
                                Ok(addr) => {
                                    match Connecting::host(addr, debug, cursor)
                                    {
                                        Ok(state) => *connecting = Some(state),
                                        Err(err) => {
                                            let err = format!(
                                                "error hosting server: {}",
                                                err
                                            );
                                            error!("{}", err);
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
                ..
            } => (),
        }
    }
}
