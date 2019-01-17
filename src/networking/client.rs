use crate::debug::{NetworkStats, NETWORK_STATS_RATE};
use crate::game::{client::Game, Input};
use crate::networking::connection::{Connection, HEADER_BYTES};
use crate::networking::event_loop::{run_event_loop, EventHandler};
use crate::networking::server::{ServerHandshake, ServerPacket};
use crate::networking::tick::Interval;
use crate::networking::{Error, RttEstimator, MAX_PACKET_SIZE, PING_RATE};
use crossbeam::channel::{self, Receiver, Sender};
use log::{error, info, trace, warn};
use mio::net::UdpSocket;
use mio::{Event, Poll, PollOpt, Ready, Registration, SetReadiness, Token};
use mio_extras::timer::{self, Timeout, Timer};
use nalgebra::Point2;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{self, Cursor};
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const SOCKET: Token = Token(0);
const TIMER: Token = Token(1);
const SHUTDOWN: Token = Token(2);

const TICK_RATE: f32 = 1.0 / 30.0;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum TimeoutState {
    Tick,
    Ping,
    UpdateStats,
    Connect,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ClientPacket {
    Input {
        sequence: u32,
        input: Input,
    },
    Disconnect,
    Ping,
    Pong(u32),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientHandshake {
    /// Cursor position when connecting.
    pub cursor: Point2<f32>,
}

pub enum ClientState {
    Connecting {
        done: Sender<Result<Arc<Game>, Error>>,
        timeout: Timeout,
        cursor: Point2<f32>,
    },
    Connected {
        tick: Interval,
        rtt: RttEstimator,
        ping: Interval,
        latest_snapshot_seq: u32,
        game: Arc<Game>,
    },
}

pub struct Client {
    socket: UdpSocket,
    timer: Timer<TimeoutState>,
    recv_buffer: [u8; MAX_PACKET_SIZE],
    send_queue: VecDeque<Vec<u8>>,
    poll: Poll,
    connection: Connection,
    state: ClientState,
    _shutdown: Registration,
    /// Marks after `shutdown` has been received, to shutdown when the
    /// `send_queue` is empty.
    needs_shutdown: bool,
    stats_tx: Sender<NetworkStats>,
    stats: NetworkStats,
}

/// Client handle used while connecting to a sever.
pub struct ClientHandle {
    shutdown: SetReadiness,
}

pub struct ConnectingHandle {
    done: Receiver<Result<Arc<Game>, Error>>,
}

pub fn connect(
    addr: SocketAddr,
    stats: Sender<NetworkStats>,
    cursor: Point2<f32>,
) -> Result<(ClientHandle, ConnectingHandle), Error> {
    let (done_tx, done_rx) = channel::bounded(1);
    let (shutdown_registration, shutdown_set_readiness) = Registration::new2();
    let mut client =
        Client::new(addr, done_tx, stats, shutdown_registration, cursor)?;
    thread::spawn(move || {
        run_event_loop(&mut client);
    });
    Ok((
        ClientHandle {
            shutdown: shutdown_set_readiness,
        },
        ConnectingHandle {
            done: done_rx,
        },
    ))
}

impl ClientHandle {
    /// Signals the client thread to shutdown.
    pub fn shutdown(&self) {
        if let Err(err) = self.shutdown.set_readiness(Ready::readable()) {
            warn!("failed to signal shutdown to client: {}", err)
        }
    }
}

impl ConnectingHandle {
    /// Gets the connection result, if connection finished.
    pub fn done(&mut self) -> Option<Result<Arc<Game>, Error>> {
        match self.done.try_recv() {
            Ok(done) => Some(done),
            Err(_) => None,
        }
    }
}

// Gracefully shutdown when the handle is lost.
impl Drop for ClientHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl EventHandler for Client {
    fn poll(&self) -> &Poll {
        &self.poll
    }

    fn handle(&mut self, event: Event) -> bool {
        match event.token() {
            SOCKET => {
                if event.readiness().is_readable() {
                    self.socket_readable();
                }
                if event.readiness().is_writable() {
                    self.socket_writable();
                }

                if self.send_queue.is_empty() && self.needs_shutdown {
                    // Finished sending all pending messages so shut
                    // down for real.
                    return true;
                }
            },
            TIMER => {
                while let Some(timeout) = self.timer.poll() {
                    match timeout {
                        TimeoutState::Ping => {
                            if let Err(err) = self.send_ping() {
                                error!("error sending ping packet: {}", err);
                            }
                        },
                        TimeoutState::Tick => {
                            if let Err(err) = self.send_tick() {
                                error!("error sending tick packet: {}", err);
                            }
                        },
                        TimeoutState::UpdateStats => {
                            if let ClientState::Connected {
                                ref rtt,
                                ..
                            } = self.state
                            {
                                if let Some(rtt) = rtt.rtt() {
                                    self.stats.rtt = rtt;
                                }
                            }
                            self.stats_tx.send(self.stats).unwrap();
                            self.stats = NetworkStats::default();
                            self.timer.set_timeout(
                                Duration::from_float_secs(f64::from(
                                    NETWORK_STATS_RATE,
                                )),
                                TimeoutState::UpdateStats,
                            );
                        },
                        TimeoutState::Connect => {
                            if let ClientState::Connecting {
                                ref mut done,
                                ..
                            } = self.state
                            {
                                let _ = done.send(Err(Error::TimedOut));
                                info!("client connection timed out");
                                return true;
                            }
                        },
                    }
                }
            },
            SHUTDOWN => {
                info!("client started shutdown");
                if let Err(err) = self.send(&ClientPacket::Disconnect) {
                    error!("failed to send disconnect packet: {}", err);
                    // If this errored, just shut down immediately.
                    return true;
                }
                self.needs_shutdown = true;
            },
            Token(_) => unreachable!(),
        }

        false
    }
}

impl Client {
    pub fn new(
        addr: SocketAddr,
        done: Sender<Result<Arc<Game>, Error>>,
        stats: Sender<NetworkStats>,
        shutdown: Registration,
        cursor: Point2<f32>,
    ) -> Result<Client, Error> {
        let socket = UdpSocket::bind(&"0.0.0.0:0".parse().unwrap())
            .map_err(Error::bind_socket)?;
        socket.connect(addr).map_err(Error::connect_socket)?;
        let mut timer = timer::Builder::default()
            .tick_duration(Duration::from_millis(10))
            .build();
        let poll = Poll::new().map_err(Error::poll_init)?;
        poll.register(&socket, SOCKET, Ready::readable(), PollOpt::edge())
            .map_err(Error::poll_register)?;
        poll.register(&timer, TIMER, Ready::readable(), PollOpt::edge())
            .map_err(Error::poll_register)?;
        poll.register(&shutdown, SHUTDOWN, Ready::readable(), PollOpt::edge())
            .map_err(Error::poll_register)?;

        let timeout =
            timer.set_timeout(Duration::from_secs(5), TimeoutState::Connect);
        timer.set_timeout(
            Duration::from_float_secs(f64::from(NETWORK_STATS_RATE)),
            TimeoutState::UpdateStats,
        );

        let mut client = Client {
            socket,
            timer,
            recv_buffer: [0; MAX_PACKET_SIZE],
            send_queue: VecDeque::new(),
            poll,
            connection: Connection::default(),
            state: ClientState::Connecting {
                done,
                timeout,
                cursor,
            },
            _shutdown: shutdown,
            stats_tx: stats,
            stats: NetworkStats::default(),
            needs_shutdown: false,
        };

        // Send handshake
        client.send(&ClientHandshake {
            cursor,
        })?;

        Ok(client)
    }

    pub fn socket_readable(&mut self) {
        loop {
            match self.socket.recv(&mut self.recv_buffer) {
                Ok(bytes_read) => {
                    self.stats.bytes_in += bytes_read as u32;
                    if let Err(err) = self.on_recv(bytes_read) {
                        error!("{}", err);
                    }
                },
                Err(err) => {
                    if err.kind() != io::ErrorKind::WouldBlock {
                        error!("error receiving packet on client: {}", err);
                    } else {
                        break;
                    }
                },
            }
        }
    }

    pub fn socket_writable(&mut self) {
        while let Some(packet) = self.send_queue.pop_front() {
            match self.socket.send(&packet) {
                Err(err) => {
                    if err.kind() != io::ErrorKind::WouldBlock {
                        error!(
                            "error sending packet from client ({:?}): {}",
                            &packet, err
                        );
                    } else {
                        break;
                    }
                },
                // Pretty sure this never happens?
                Ok(bytes_written) => {
                    self.stats.bytes_out += bytes_written as u32;
                    if bytes_written < packet.len() {
                        error!(
                            "Only wrote {} out of {} bytes for client packet: \
                             {:?}",
                            bytes_written,
                            packet.len(),
                            &packet
                        )
                    }
                },
            }
        }

        if self.send_queue.is_empty() {
            // No longer care about writable events if there are no
            // more packets to send.
            if let Err(err) = self
                .poll
                .reregister(
                    &self.socket,
                    SOCKET,
                    Ready::readable(),
                    PollOpt::edge(),
                )
                .map_err(Error::poll_register)
            {
                error!("{}", err);
            }
        }
    }

    fn send_ping(&mut self) -> Result<(), Error> {
        let sequence = self.send(&ClientPacket::Ping)?;
        match self.state {
            ClientState::Connected {
                ref mut ping,
                ref mut rtt,
                ..
            } => {
                let now = Instant::now();
                let (_, interval) = ping.next(now);
                self.timer.set_timeout(interval, TimeoutState::Ping);
                rtt.ping(sequence, now);
            },
            _ => unreachable!(),
        }
        Ok(())
    }

    fn send_tick(&mut self) -> Result<(), Error> {
        match self.state {
            ClientState::Connected {
                ref mut tick,
                ref game,
                ..
            } => {
                let now = Instant::now();
                let (_, interval) = tick.next(now);
                self.timer.set_timeout(interval, TimeoutState::Tick);

                let mut input_buffer = game.input_buffer.lock();
                let (sequence, _) = input_buffer.packet_send();
                let packet = ClientPacket::Input {
                    input: *input_buffer.latest(),
                    sequence,
                };
                drop(input_buffer);

                trace!("sending tick packet to server: {:?}", packet);
                self.send(&packet)?;
            },
            // We shouldn't really be sending ticks in any other state.
            _ => unreachable!(),
        }

        Ok(())
    }

    fn on_recv(&mut self, bytes_read: usize) -> Result<(), Error> {
        // Make sure that it fits in recv_buffer
        if bytes_read > MAX_PACKET_SIZE {
            return Err(Error::PacketTooLarge(bytes_read));
        }
        let packet = &self.recv_buffer[0..bytes_read];

        let transition = match self.state {
            ClientState::Connecting {
                ref mut done,
                ref timeout,
                ref cursor,
            } => {
                // Assumed to be a handshake packet.
                let (handshake, sequence, _) =
                    self.connection
                        .decode::<_, ServerHandshake>(Cursor::new(packet))?;
                let game = Arc::new(Game::new(
                    handshake.players,
                    handshake.snapshot,
                    handshake.id,
                    *cursor,
                ));
                let tick = Interval::new(Duration::from_float_secs(f64::from(
                    TICK_RATE,
                )));
                let ping = Interval::new(Duration::from_float_secs(f64::from(
                    PING_RATE,
                )));
                // Start the timer for sending input ticks and pings.
                self.timer.cancel_timeout(timeout);
                self.timer.set_timeout(tick.interval(), TimeoutState::Tick);
                self.timer.set_timeout(ping.interval(), TimeoutState::Ping);

                // Signal the main thread that connection finished.
                done.send(Ok(game.clone())).unwrap();

                info!("completed connection to server");
                // Transition to connected state.
                Some(ClientState::Connected {
                    game,
                    tick,
                    ping,
                    rtt: RttEstimator::default(),
                    latest_snapshot_seq: sequence,
                })
            },

            ClientState::Connected {
                ref mut game,
                ref mut latest_snapshot_seq,
                ref mut rtt,
                ..
            } => {
                let (packet, sequence, _) =
                    self.connection.decode(Cursor::new(packet))?;
                match packet {
                    ServerPacket::Snapshot {
                        snapshot,
                        last_input: (input_sequence, input_delay),
                    } => {
                        game.input_buffer.lock().packet_ack(input_sequence);
                        trace!("got snapshot from server");
                        // Only process snapshots that are newer than
                        // the last one. Out of order snapshots are
                        // dropped.
                        if sequence > *latest_snapshot_seq {
                            game.insert_snapshot(snapshot, input_delay);
                            *latest_snapshot_seq = sequence;
                        }
                    },
                    ServerPacket::PlayerJoined {
                        id,
                        static_state,
                    } => {
                        info!("new player joined: {}", id);
                        game.add_player(id, static_state);
                    },
                    ServerPacket::PlayerLeft(id) => {
                        info!("player {} left", id);
                        game.remove_player(id);
                    },
                    ServerPacket::Pong(sequence) => {
                        rtt.pong(sequence);
                    },
                    ServerPacket::Ping => {
                        self.send(&ClientPacket::Pong(sequence))?;
                    },
                }

                None
            },
        };

        if let Some(transition) = transition {
            self.state = transition;
        }

        Ok(())
    }

    fn send<P: Serialize>(&mut self, contents: &P) -> Result<u32, Error> {
        // Don't send any additional packets while shutting down.
        if self.needs_shutdown {
            return Err(Error::ShuttingDown);
        }

        let size = bincode::serialized_size(contents)
            .map_err(Error::serialize)? as usize;
        let mut packet = Vec::with_capacity(size + HEADER_BYTES);
        let sequence = self.connection.send_header(&mut packet)?;
        bincode::serialize_into(&mut packet, contents)
            .map_err(Error::serialize)?;
        self.send_queue.push_back(packet);
        self.poll
            .reregister(
                &self.socket,
                SOCKET,
                Ready::readable() | Ready::writable(),
                PollOpt::edge(),
            )
            .map_err(Error::poll_register)?;

        Ok(sequence)
    }
}
