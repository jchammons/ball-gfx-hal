use crate::debug::{NetworkStats, NETWORK_STATS_RATE};
use crate::game::{
    client::{Game, GameHandle},
    GameSettings,
    Input,
};
use crate::networking::connection::{Connection, HEADER_BYTES};
use crate::networking::event_loop::{run_event_loop, EventHandler};
use crate::networking::server::ServerPacket;
use crate::networking::tick::Interval;
use crate::networking::{
    Error,
    RecvError,
    RttEstimator,
    CONNECTION_TIMEOUT,
    MAX_PACKET_SIZE,
    PING_RATE,
};
use crossbeam::channel::{self, Receiver, Sender};
use log::{debug, error, info, trace, warn};
use mio::net::UdpSocket;
use mio::{Event, Poll, PollOpt, Ready, Registration, SetReadiness, Token};
use mio_extras::timer::{self, Timeout, Timer};
use nalgebra::Point2;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Cursor};
use std::net::SocketAddr;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const SOCKET: Token = Token(0);
const TIMER: Token = Token(1);
const SHUTDOWN: Token = Token(2);

const TICK_RATE: Duration = Duration::from_millis(15);

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum TimeoutState {
    Tick,
    Ping,
    UpdateStats,
    LostConnection,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ClientPacket {
    Handshake {
        /// Cursor position when connecting.
        cursor: Point2<f32>,
    },
    Settings(GameSettings),
    Input(Input),
    Disconnect,
    Ping,
    Pong(u32),
}

pub enum ClientState {
    Connecting {
        done: Sender<Result<(Game, ConnectedHandle), Option<Error>>>,
        cursor: Point2<f32>,
    },
    Connected {
        done: Sender<Option<Error>>,
        tick: Interval,
        rtt: RttEstimator,
        ping: Interval,
        game: GameHandle,
    },
}

struct Stats {
    send: Sender<NetworkStats>,
    next: NetworkStats,
}

pub struct Client {
    socket: UdpSocket,
    timer: Timer<TimeoutState>,
    recv_buffer: [u8; MAX_PACKET_SIZE],
    send_queue: VecDeque<Vec<u8>>,
    poll: Poll,
    timeout: Timeout,
    connection: Connection,
    reliable: HashMap<u32, ClientPacket>,
    state: ClientState,
    _shutdown: Registration,
    /// Marks after `shutdown` has been received, to shutdown when the
    /// `send_queue` is empty.
    needs_shutdown: bool,
    stats: Option<Stats>,
}

pub type ConnectingHandle =
    Receiver<Result<(Game, ConnectedHandle), Option<Error>>>;

pub type ConnectedHandle = Receiver<Option<Error>>;

/// Client handle used while connecting to a sever.
pub struct ClientHandle {
    shutdown: SetReadiness,
}

pub fn connect(
    addr: SocketAddr,
    stats: Option<Sender<NetworkStats>>,
    cursor: Point2<f32>,
) -> Result<(ClientHandle, ConnectingHandle, JoinHandle<()>), Error> {
    let (done_tx, done_rx) = channel::bounded(1);
    let (shutdown_registration, shutdown_set_readiness) = Registration::new2();
    let client =
        Client::new(addr, done_tx, stats, shutdown_registration, cursor)?;
    let thread = thread::spawn(move || {
        run_event_loop(client);
        info!("client done");
    });
    Ok((
        ClientHandle {
            shutdown: shutdown_set_readiness,
        },
        done_rx,
        thread,
    ))
}

impl ClientHandle {
    /// Signals the client thread to shutdown.
    pub fn shutdown(&self) {
        if let Err(err) = self.shutdown.set_readiness(Ready::readable()) {
            error!("failed to signal shutdown to client: {}", err)
        }
    }
}

impl Drop for ClientHandle {
    /// Gracefully shutdown when the handle is lost.
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl ClientPacket {
    fn reliable(&self) -> bool {
        match self {
            ClientPacket::Handshake {
                ..
            } => true,
            ClientPacket::Settings(_) => true,
            ClientPacket::Input(_) => false,
            ClientPacket::Disconnect => false,
            ClientPacket::Ping => false,
            ClientPacket::Pong(_) => false,
        }
    }

    fn resend(&self, game: Option<&GameHandle>) -> bool {
        match self {
            ClientPacket::Handshake {
                ..
            } => true,
            ClientPacket::Settings(settings) => {
                &game.as_ref().unwrap().settings.settings() == settings
            },
            _ => self.reliable(),
        }
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
                    // Don't process any new messages while shutting down.
                    if self.needs_shutdown {
                        return false;
                    }

                    if let Err(err) = self.socket_readable() {
                        return self.start_shutdown(Some(err));
                    }
                }
                if event.readiness().is_writable() {
                    if let Err(err) = self.socket_writable() {
                        return self.start_shutdown(Some(err));
                    }
                }

                if self.send_queue.is_empty() && self.needs_shutdown {
                    // Finished sending all pending messages so shut
                    // down for real.
                    return true;
                }
            },
            TIMER => {
                // Don't respond to timer events while shutting down.
                if self.needs_shutdown {
                    return false;
                }

                while let Some(timeout) = self.timer.poll() {
                    match timeout {
                        TimeoutState::Ping => {
                            if let Err(err) = self.send_ping() {
                                return self.start_shutdown(Some(err));
                            }
                        },
                        TimeoutState::Tick => {
                            if let Err(err) = self.send_tick() {
                                return self.start_shutdown(Some(err));
                            }
                        },
                        TimeoutState::UpdateStats => {
                            // If this timeout is triggered with
                            // stats=None, it is a bug and should
                            // crash.
                            let stats = self.stats.as_mut().unwrap();

                            if let ClientState::Connected {
                                ref rtt,
                                ..
                            } = self.state
                            {
                                if let Some(rtt) = rtt.rtt() {
                                    stats.next.rtt = rtt;
                                }
                            }
                            stats.send.send(stats.next).unwrap();
                            stats.next = NetworkStats::default();
                            self.timer.set_timeout(
                                NETWORK_STATS_RATE,
                                TimeoutState::UpdateStats,
                            );
                        },
                        TimeoutState::LostConnection => {
                            return self.start_shutdown(Some(Error::TimedOut));
                        },
                    }
                }
            },
            SHUTDOWN => {
                info!("client started shutdown");
                return self.start_shutdown(None);
            },
            Token(_) => unreachable!(),
        }

        false
    }
}

impl Client {
    pub fn new(
        addr: SocketAddr,
        done: Sender<Result<(Game, ConnectedHandle), Option<Error>>>,
        stats: Option<Sender<NetworkStats>>,
        shutdown: Registration,
        cursor: Point2<f32>,
    ) -> Result<Client, Error> {
        let socket =
            UdpSocket::bind(&"0.0.0.0:0".parse().unwrap()).map_err(|err| {
                Error::BindSocket {
                    addr,
                    err,
                }
            })?;
        socket.connect(addr).map_err(|err| {
            Error::ConnectSocket {
                addr,
                err,
            }
        })?;
        let mut timer = timer::Builder::default()
            .tick_duration(Duration::from_millis(10))
            .build();
        let poll = Poll::new().map_err(Error::poll)?;
        poll.register(&socket, SOCKET, Ready::readable(), PollOpt::edge())
            .map_err(Error::poll)?;
        poll.register(&timer, TIMER, Ready::readable(), PollOpt::edge())
            .map_err(Error::poll)?;
        poll.register(&shutdown, SHUTDOWN, Ready::readable(), PollOpt::edge())
            .map_err(Error::poll)?;

        let timeout =
            timer.set_timeout(CONNECTION_TIMEOUT, TimeoutState::LostConnection);
        if stats.is_some() {
            timer.set_timeout(NETWORK_STATS_RATE, TimeoutState::UpdateStats);
        }

        let mut client = Client {
            socket,
            timer,
            recv_buffer: [0; MAX_PACKET_SIZE],
            send_queue: VecDeque::new(),
            poll,
            timeout,
            connection: Connection::default(),
            reliable: HashMap::new(),
            state: ClientState::Connecting {
                done,
                cursor,
            },
            _shutdown: shutdown,
            stats: stats.map(|send| {
                Stats {
                    send,
                    next: NetworkStats::default(),
                }
            }),
            needs_shutdown: false,
        };

        // Send handshake
        client.send(&ClientPacket::Handshake {
            cursor,
        })?;

        Ok(client)
    }

    /// Starts shutting down the networking thread, with a provided reason.
    ///
    /// If any errors occur at this point, returns `true` to indicate
    /// that the event loop should hard-shutdown immediately.
    #[must_use]
    fn start_shutdown(&mut self, reason: Option<Error>) -> bool {
        // If already shutting down, don't redo this stuff.
        if self.needs_shutdown {
            return true;
        }

        match self.state {
            ClientState::Connecting {
                ref mut done,
                ..
            } => {
                let _ = done.send(Err(reason));
            },
            ClientState::Connected {
                ref mut done,
                ..
            } => {
                let _ = done.send(reason);
            },
        }
        // Get rid of any pending packets.
        self.send_queue.clear();
        // Send off a bunch of disconnected packets to the server, in
        // the hopes that at least one gets through.
        for _ in 0..8 {
            if let Err(err) = self.send(&ClientPacket::Disconnect) {
                error!(
                    "error ocurred while sending disconnect packets: {}",
                    err
                );
                return true;
            }
        }
        self.needs_shutdown = true;
        false
    }

    fn reregister_socket(&mut self, writable: bool) -> Result<(), Error> {
        let readiness = if writable {
            Ready::readable() | Ready::writable()
        } else {
            Ready::readable()
        };

        self.poll
            .reregister(&self.socket, SOCKET, readiness, PollOpt::edge())
            .map_err(Error::poll)
    }

    fn socket_readable(&mut self) -> Result<(), Error> {
        loop {
            match self.socket.recv(&mut self.recv_buffer) {
                Ok(bytes_read) => {
                    // Reset the connection timeout.
                    self.timer.cancel_timeout(&self.timeout);
                    self.timeout = self.timer.set_timeout(
                        CONNECTION_TIMEOUT,
                        TimeoutState::LostConnection,
                    );
                    // Handle packet.
                    if let Some(ref mut stats) = self.stats {
                        stats.next.bytes_in += bytes_read as u32;
                    }
                    if let Err(err) = self.on_recv(bytes_read)? {
                        error!(
                            "receiving packet failed ({:?}): {}",
                            &self.recv_buffer[0..bytes_read],
                            err
                        );
                    }
                },
                Err(err) => {
                    if err.kind() != io::ErrorKind::WouldBlock {
                        error!("error receiving packet on client: {}", err);
                        return Err(Error::SocketRead(err));
                    } else {
                        break;
                    }
                },
            }
        }

        Ok(())
    }

    fn socket_writable(&mut self) -> Result<(), Error> {
        while let Some(packet) = self.send_queue.pop_front() {
            match self.socket.send(&packet) {
                Err(err) => {
                    if err.kind() != io::ErrorKind::WouldBlock {
                        error!(
                            "error sending packet from client ({:?}): {}",
                            &packet, err
                        );
                        return Err(Error::SocketWrite(err));
                    } else {
                        break;
                    }
                },
                Ok(bytes_written) => {
                    if let Some(ref mut stats) = self.stats {
                        stats.next.packets_sent += 1;
                        stats.next.bytes_out += bytes_written as u32;
                    }
                    // Pretty sure this never happens?
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
            self.reregister_socket(false)?;
        }
        Ok(())
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

                let tick_packet = ClientPacket::Input(game.latest_input());
                trace!("sending tick packet to server: {:?}", tick_packet);

                // If the settings have changed, send that as well.
                if let Some(settings) = game.settings.dirty() {
                    let settings_packet = ClientPacket::Settings(settings);
                    trace!(
                        "sending settings update packet to server: {:?}",
                        settings_packet
                    );
                    self.send(&settings_packet)?;
                }

                self.send(&tick_packet)?;
            },
            // We shouldn't really be sending ticks in any other state.
            _ => unreachable!(),
        }

        Ok(())
    }

    fn on_recv(
        &mut self,
        bytes_read: usize,
    ) -> Result<Result<(), RecvError>, Error> {
        // Make sure that it fits in recv_buffer
        if bytes_read > MAX_PACKET_SIZE {
            return Ok(Err(RecvError::PacketTooLarge(bytes_read)));
        }
        let packet = &self.recv_buffer[0..bytes_read];
        let (packet, sequence, acks, lost) =
            match self.connection.decode(Cursor::new(packet)) {
                Ok(result) => result,
                Err(err) => return Ok(Err(err)),
            };

        if let Some(ref mut stats) = self.stats {
            stats.next.packets_lost += lost.len() as u16;
        }

        // Remove acked packets from the reliable packet buffer.
        for ack in acks.iter() {
            self.reliable.remove(&ack);
        }

        // Possibly resend any lost packets.
        for lost in lost.into_iter() {
            if let Some(packet) = self.reliable.remove(&lost) {
                let game = match self.state {
                    ClientState::Connecting {
                        ..
                    } => None,
                    ClientState::Connected {
                        ref game,
                        ..
                    } => Some(game),
                };
                if packet.resend(game) {
                    debug!("resending lost packet from client: {:?}", packet);
                    self.send(&packet)?;
                }
            }
        }

        let transition = match self.state {
            ClientState::Connecting {
                ref mut done,
                ref cursor,
            } => {
                match packet {
                    ServerPacket::Handshake {
                        players,
                        round,
                        round_duration,
                        settings,
                        snapshot,
                        id,
                    } => {
                        let (game, game_handle) = Game::new(
                            players,
                            snapshot,
                            round,
                            round_duration,
                            settings,
                            id,
                            *cursor,
                        );
                        let tick = Interval::new(TICK_RATE);
                        let ping = Interval::new(PING_RATE);
                        // Start the timer for sending input ticks and pings.
                        self.timer
                            .set_timeout(tick.interval(), TimeoutState::Tick);
                        self.timer
                            .set_timeout(ping.interval(), TimeoutState::Ping);

                        // Signal the main thread that connection finished.
                        let (done_tx, done_rx) = channel::bounded(1);
                        let _ = done.send(Ok((game, done_rx)));

                        info!("completed connection to server");
                        // Transition to connected state.
                        Some(ClientState::Connected {
                            done: done_tx,
                            game: game_handle,
                            tick,
                            ping,
                            rtt: RttEstimator::default(),
                        })
                    },
                    // Ignore non-handshake packets
                    _ => {
                        warn!("received {:?} before handshake", packet);
                        None
                    },
                }
            },
            ClientState::Connected {
                ref mut game,
                ref mut rtt,
                ..
            } => {
                match packet {
                    ServerPacket::Event(event) => game.event(event),
                    ServerPacket::Handshake {
                        ..
                    } => warn!("received a second handshake packet"),
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

        Ok(Ok(()))
    }

    fn send(&mut self, contents: &ClientPacket) -> Result<u32, Error> {
        // Don't send any additional packets while shutting down.
        if self.needs_shutdown {
            panic!("attempted to send packet while already shutting down");
        }

        // Serialization errors are always fatal.
        let size = bincode::serialized_size(contents).unwrap() as usize;
        let mut packet = Vec::with_capacity(size + HEADER_BYTES);
        let sequence = self.connection.send_header(&mut packet);
        bincode::serialize_into(&mut packet, contents).unwrap();
        self.send_queue.push_back(packet);
        self.reregister_socket(true)?;

        if contents.reliable() {
            self.reliable.insert(sequence, contents.clone());
        }

        Ok(sequence)
    }
}
