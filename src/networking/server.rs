use crate::game::{
    clamp_cursor,
    server::Game,
    Event,
    GetPlayer,
    PlayerId,
    Snapshot,
    StaticPlayerState,
};
use crate::networking::client::{ClientHandshake, ClientPacket};
use crate::networking::connection::{Connection, HEADER_BYTES};
use crate::networking::event_loop::{run_event_loop, EventHandler};
use crate::networking::tick::Interval;
use crate::networking::{
    Error,
    RttEstimator,
    CONNECTION_TIMEOUT,
    MAX_PACKET_SIZE,
    PING_RATE,
    SNAPSHOT_RATE,
};
use log::{debug, error, info, trace, warn};
use mio::net::UdpSocket;
use mio::{self, Poll, PollOpt, Ready, Registration, SetReadiness, Token};
use mio_extras::timer::{self, Timeout, Timer};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::io::Cursor;
use std::iter;
use std::net::SocketAddr;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const TICK_RATE: f32 = 1.0 / 60.0;

const SOCKET: Token = Token(0);
const TIMER: Token = Token(1);
const SHUTDOWN: Token = Token(2);

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum TimeoutState {
    SendSnapshot,
    Tick,
    Ping,
    LostConnection(SocketAddr),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServerPacket {
    Event(Event),
    Ping,
    Pong(u32),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerHandshake {
    pub id: PlayerId,
    pub players: HashMap<PlayerId, StaticPlayerState>,
    pub snapshot: Snapshot,
}

struct Client {
    player: PlayerId,
    connection: Connection,
    timeout: Timeout,
    rtt: RttEstimator,
}

/// Contains a message and a list of clients to send the message to.
struct Packet {
    /// Current client with the correct header.
    pub client: SocketAddr,
    /// Clients left to send to.
    remaining: Vec<SocketAddr>,
    /// The packet is assumed to have room reserved at the front for a
    /// header. Every time a new client is sent to, these bytes get
    /// overwritten with the new header. This means that the previous
    /// `packet` stops being correct.
    pub packet: Vec<u8>,
}

pub struct Server {
    socket: UdpSocket,
    timer: Timer<TimeoutState>,
    recv_buffer: [u8; MAX_PACKET_SIZE],
    send_queue: VecDeque<Packet>,
    clients: HashMap<SocketAddr, Client>,
    game: Game,
    send_tick: Interval,
    game_tick: Interval,
    ping: Interval,
    poll: Poll,
    _shutdown: Registration,
}

pub struct ServerHandle {
    pub shutdown: SetReadiness,
}

/// Launches a server bound to a particular address.
pub fn host(addr: SocketAddr) -> Result<(ServerHandle, JoinHandle<()>), Error> {
    let (shutdown_registration, shutdown_set_readiness) = Registration::new2();
    let mut server = Server::new(&addr, shutdown_registration)?;
    let thread = thread::spawn(move || {
        run_event_loop(&mut server);
    });
    Ok((
        ServerHandle {
            shutdown: shutdown_set_readiness,
        },
        thread,
    ))
}

impl ServerHandle {
    /// Attmepts to signal the associated server to shutdown.
    pub fn shutdown(&self) {
        if let Err(err) = self.shutdown.set_readiness(Ready::readable()) {
            warn!("failed to signal shutdown to server: {}", err)
        }
    }
}

// Gracefully shut down when the handle is dropped.
impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Packet {
    /// Constructs a new packet for a single client.
    pub fn single_client<P: Serialize>(
        addr: SocketAddr,
        client: &mut Client,
        contents: &P,
    ) -> Result<(Packet, u32), Error> {
        // Write header and contents
        let size = bincode::serialized_size(contents)
            .map_err(Error::serialize)? as usize;
        let mut packet = Vec::with_capacity(size + HEADER_BYTES);
        let sequence = client.connection.send_header(&mut packet)?;
        bincode::serialize_into(&mut packet, contents)
            .map_err(Error::serialize)?;

        Ok((
            Packet {
                client: addr,
                remaining: Vec::new(),
                packet,
            },
            sequence,
        ))
    }

    /// Poossibly constructs a new packet, but returns `None` if
    /// `clients` is empty.
    pub fn new<I: IntoIterator<Item = SocketAddr>, P: Serialize>(
        clients: I,
        contents: &P,
        clients_state: &mut HashMap<SocketAddr, Client>,
    ) -> Result<Option<Packet>, Error> {
        // Determine first client, to write the header for.
        let mut clients = clients.into_iter();
        let client = match clients.next() {
            Some(client) => client,
            None => return Ok(None),
        };

        // Write header and contents
        let size = bincode::serialized_size(contents)
            .map_err(Error::serialize)? as usize;
        let mut packet = Vec::with_capacity(size + HEADER_BYTES);
        let client_state = clients_state.get_mut(&client).unwrap();
        client_state.connection.send_header(&mut packet)?;
        bincode::serialize_into(&mut packet, contents)
            .map_err(Error::serialize)?;

        Ok(Some(Packet {
            client,
            remaining: clients.collect(),
            packet,
        }))
    }

    pub fn next_packet(
        &mut self,
        clients_state: &mut HashMap<SocketAddr, Client>,
    ) -> Result<bool, Error> {
        match self.remaining.pop() {
            Some(client) => {
                self.client = client;
                // Write the new header.
                let connection =
                    &mut clients_state.get_mut(&client).unwrap().connection;
                let cursor = Cursor::new(&mut self.packet[..HEADER_BYTES]);
                connection.send_header(cursor)?;
                Ok(true)
            },
            None => Ok(false),
        }
    }
}

impl EventHandler for Server {
    fn poll(&self) -> &Poll {
        &self.poll
    }

    fn handle(&mut self, event: mio::Event) -> bool {
        match event.token() {
            SOCKET => {
                if event.readiness().is_readable() {
                    self.socket_readable();
                }
                if event.readiness().is_writable() {
                    self.socket_writable();
                }
            },
            TIMER => {
                while let Some(timeout) = self.timer.poll() {
                    match timeout {
                        TimeoutState::SendSnapshot => {
                            self.send_snapshot();
                        },
                        TimeoutState::Tick => {
                            self.game_tick();
                        },
                        TimeoutState::Ping => {
                            self.send_ping();
                        },
                        TimeoutState::LostConnection(addr) => {
                            info!("client from {} timed out", addr);
                            if let Err(err) = self.remove_client(&addr) {
                                error!(
                                    "failed to remove client from {}: {}",
                                    addr, err
                                );
                            }
                        },
                    }
                }
            },
            SHUTDOWN => {
                info!("server received shutdown from handle");
                return true;
            },
            _ => unreachable!(),
        }

        false
    }
}

impl Server {
    pub fn new(
        addr: &SocketAddr,
        shutdown: Registration,
    ) -> Result<Server, Error> {
        let socket = UdpSocket::bind(addr).map_err(Error::bind_socket)?;
        let mut timer = timer::Builder::default()
            .tick_duration(Duration::from_millis(5))
            .build();
        let poll = Poll::new().map_err(Error::poll_init)?;
        poll.register(&socket, SOCKET, Ready::readable(), PollOpt::edge())
            .map_err(Error::poll_register)?;
        poll.register(&timer, TIMER, Ready::readable(), PollOpt::edge())
            .map_err(Error::poll_register)?;
        poll.register(&shutdown, SHUTDOWN, Ready::readable(), PollOpt::edge())
            .map_err(Error::poll_register)?;

        // Set timeout for the first tick. All subsequent ticks will
        // be generated from Server::send_tick.
        let send_tick =
            Interval::new(Duration::from_float_secs(f64::from(SNAPSHOT_RATE)));
        timer.set_timeout(send_tick.interval(), TimeoutState::SendSnapshot);
        let game_tick =
            Interval::new(Duration::from_float_secs(f64::from(TICK_RATE)));
        timer.set_timeout(game_tick.interval(), TimeoutState::Tick);
        let ping =
            Interval::new(Duration::from_float_secs(f64::from(PING_RATE)));
        timer.set_timeout(ping.interval(), TimeoutState::Ping);

        Ok(Server {
            socket,
            timer,
            recv_buffer: [0; MAX_PACKET_SIZE],
            send_queue: VecDeque::new(),
            clients: HashMap::new(),
            game: Game::default(),
            send_tick,
            game_tick,
            ping,
            poll,
            _shutdown: shutdown,
        })
    }

    fn socket_readable(&mut self) {
        // Attempt to read packets until recv_from returns WouldBlock.
        loop {
            match self.socket.recv_from(&mut self.recv_buffer) {
                Err(err) => {
                    if err.kind() != io::ErrorKind::WouldBlock {
                        error!("error receiving packet: {}", err);
                    } else {
                        break;
                    }
                },
                Ok((bytes_read, addr)) => {
                    if let Err(err) = self.on_recv(addr, bytes_read) {
                        error!("error receiving packet from {}: {}", addr, err);
                    }
                },
            }
        }
    }

    fn socket_writable(&mut self) {
        while let Some(packet) = self.send_queue.front_mut() {
            match self.socket.send_to(&packet.packet, &packet.client) {
                Err(err) => {
                    if err.kind() != io::ErrorKind::WouldBlock {
                        error!(
                            "error sending packet to {} ({}): {:?}",
                            &packet.client, err, &packet.packet
                        );
                    }
                    break;
                },
                // Pretty sure this never happens?
                Ok(bytes_written) => {
                    if bytes_written < packet.packet.len() {
                        error!(
                            "only wrote {} out of {} bytes for packet to {}: \
                             {:?}",
                            bytes_written,
                            packet.packet.len(),
                            &packet.client,
                            &packet.packet
                        )
                    }
                },
            }

            // If it got here, the packet must have actually been sent.
            match packet.next_packet(&mut self.clients) {
                Ok(true) => {
                    // Continue as usual.
                },
                Ok(false) => {
                    self.send_queue.pop_front().unwrap();
                },
                Err(err) => {
                    error!("error writing header for packet: {}", err);
                },
            }
        }

        if self.send_queue.is_empty() {
            // No longer care about writable events if there are no
            // more packets to send.
            if let Err(err) = self.reregister_socket(false) {
                error!("{}", err);
            }
        }
    }

    fn send_events<E: Iterator<Item = Event>>(
        &mut self,
        events: E,
    ) -> Result<(), Error> {
        for event in events {
            self.broadcast(&ServerPacket::Event(event))?;
        }
        Ok(())
    }

    fn send_ping(&mut self) {
        let now = Instant::now();
        let (_, interval) = self.ping.next(now);
        self.timer.set_timeout(interval, TimeoutState::Ping);

        for (&addr, client) in self.clients.iter_mut() {
            let (packet, sequence) = match Packet::single_client(
                addr,
                client,
                &ServerPacket::Ping,
            ) {
                Ok(result) => result,
                Err(err) => {
                    error!(
                        "error initializing ping packet for client {}: {}",
                        addr, err
                    );
                    continue;
                },
            };
            self.send_queue.push_back(packet);
            client.rtt.ping(sequence, now);
        }
    }

    fn send_snapshot(&mut self) {
        // Send a snapshot to all connected clients.
        let now = Instant::now();
        let (_, interval) = self.send_tick.next(now);
        self.timer.set_timeout(interval, TimeoutState::SendSnapshot);

        let snapshot = self.game.snapshot();
        trace!("sending snapshot: {:#?}", snapshot);
        if let Err(err) =
            self.send_events(iter::once(Event::Snapshot(snapshot)))
        {
            error!("error sending snapshot: {}", err);
        }
    }

    fn game_tick(&mut self) {
        let now = Instant::now();
        let (dt, interval) = self.game_tick.next(now);
        let dt = dt.as_float_secs() as f32;
        self.timer.set_timeout(interval, TimeoutState::Tick);

        debug!("stepping game tick (dt={})", dt);
        let events = self.game.tick(dt);
        if let Err(err) = self.send_events(events) {
            error!("error sending events: {}", err);
        }
    }

    fn new_client(
        &mut self,
        addr: SocketAddr,
        connection: Connection,
        handshake: &ClientHandshake,
    ) -> Result<(), Error> {
        info!("new player from {}", addr);

        let timeout = self.timer.set_timeout(
            Duration::from_float_secs(f64::from(CONNECTION_TIMEOUT)),
            TimeoutState::LostConnection(addr),
        );

        let (player_id, events) =
            self.game.add_player(clamp_cursor(handshake.cursor));
        self.send_events(events)?;

        // Now start processing this client.
        self.clients.insert(
            addr,
            Client {
                timeout,
                player: player_id,
                connection,
                rtt: RttEstimator::default(),
            },
        );

        // Send handshake message to the new client.
        let packet = ServerHandshake {
            id: player_id,
            players: self
                .game
                .players()
                .map(|(id, player)| (id, player.static_state().clone()))
                .collect(),
            snapshot: self.game.snapshot(),
        };
        self.send_to(addr, &packet)?;

        Ok(())
    }

    fn remove_client(&mut self, addr: &SocketAddr) -> Result<(), Error> {
        if let Some(client) = self.clients.remove(addr) {
            info!("player {} from {} left", client.player, addr);
            let events = self.game.remove_player(client.player);
            self.send_events(events)?;
        }

        Ok(())
    }

    fn on_recv(
        &mut self,
        addr: SocketAddr,
        bytes_read: usize,
    ) -> Result<(), Error> {
        if bytes_read > MAX_PACKET_SIZE {
            return Err(Error::PacketTooLarge(bytes_read));
        }
        let packet = &self.recv_buffer[0..bytes_read];
        trace!("got packet from {}: {:?}", addr, &packet);
        match self.clients.get_mut(&addr) {
            Some(client) => {
                // Reset timeout.
                self.timer.cancel_timeout(&client.timeout);
                client.timeout = self.timer.set_timeout(
                    Duration::from_float_secs(f64::from(CONNECTION_TIMEOUT)),
                    TimeoutState::LostConnection(addr),
                );
                // Existing player.
                let (packet, sequence, _) =
                    client.connection.decode(Cursor::new(packet))?;
                match packet {
                    ClientPacket::Input(input) => {
                        self.game.set_player_cursor(
                            client.player,
                            clamp_cursor(input.cursor),
                        );
                    },
                    ClientPacket::Disconnect => {
                        self.remove_client(&addr)?;
                    },
                    ClientPacket::Ping => {
                        self.send_queue.push_back(
                            Packet::single_client(
                                addr,
                                client,
                                &ServerPacket::Pong(sequence),
                            )?
                            .0,
                        );
                    },
                    ClientPacket::Pong(sequence) => {
                        client.rtt.pong(sequence);
                    },
                }
            },
            None => {
                // New player.
                let mut connection = Connection::default();
                let (handshake, ..) = connection.decode(Cursor::new(packet))?;
                self.new_client(addr, connection, &handshake)?;
            },
        }

        Ok(())
    }

    fn reregister_socket(&mut self, writable: bool) -> Result<(), Error> {
        let readiness = if writable {
            Ready::readable() | Ready::writable()
        } else {
            Ready::readable()
        };

        self.poll
            .reregister(&self.socket, SOCKET, readiness, PollOpt::edge())
            .map_err(Error::poll_register)
    }

    fn send_to<P: Serialize>(
        &mut self,
        addr: SocketAddr,
        packet: &P,
    ) -> Result<(), Error> {
        self.send_queue.push_back(
            Packet::new(Some(addr), packet, &mut self.clients)?.unwrap(),
        );
        self.reregister_socket(true)?;
        Ok(())
    }

    fn broadcast<P: Serialize>(&mut self, packet: &P) -> Result<(), Error> {
        let packet = Packet::new(
            self.clients.keys().cloned().collect::<Vec<_>>(),
            packet,
            &mut self.clients,
        )?;
        if let Some(packet) = packet {
            self.send_queue.push_back(packet);
            self.reregister_socket(true)?;
        }
        Ok(())
    }
}
