use crate::game::{
    clamp_cursor,
    server::Game,
    Event,
    GameSettings,
    GetPlayer,
    PlayerId,
    RoundState,
    Snapshot,
    StaticPlayerState,
};
use crate::networking::client::ClientPacket;
use crate::networking::connection::{Connection, HEADER_BYTES};
use crate::networking::event_loop::{run_event_loop, EventHandler};
use crate::networking::tick::Interval;
use crate::networking::{
    Error,
    RecvError,
    RttEstimator,
    CONNECTION_TIMEOUT,
    MAX_PACKET_SIZE,
    PING_RATE,
    SNAPSHOT_RATE,
};
use crossbeam::channel::{self, Receiver, Sender};
use log::{debug, error, info, trace, warn};
use mio::net::UdpSocket;
use mio::{self, Poll, PollOpt, Ready, Registration, SetReadiness, Token};
use mio_extras::timer::{self, Timeout, Timer};
use nalgebra::Point2;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::io::Cursor;
use std::iter;
use std::net::SocketAddr;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const TICK_RATE: Duration = Duration::from_millis(15);

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
    Handshake {
        id: PlayerId,
        settings: GameSettings,
        players: HashMap<PlayerId, StaticPlayerState>,
        round: RoundState,
        round_duration: f32,
        snapshot: Snapshot,
    },
}

struct Client {
    player: PlayerId,
    connection: Connection,
    timeout: Timeout,
    rtt: RttEstimator,
    last_input: u32,
    reliable: HashMap<u32, ServerPacket>,
}

pub struct Server {
    socket: UdpSocket,
    timer: Timer<TimeoutState>,
    recv_buffer: [u8; MAX_PACKET_SIZE],
    send_queue: VecDeque<(SocketAddr, Vec<u8>)>,
    clients: HashMap<SocketAddr, Client>,
    game: Game,
    send_tick: Interval,
    game_tick: Interval,
    ping: Interval,
    poll: Poll,
    done: Sender<Option<Error>>,
    _shutdown: Registration,
}

pub struct ServerHandle {
    shutdown: SetReadiness,
    pub done: Receiver<Option<Error>>,
}

/// Launches a server bound to a particular address.
pub fn host(addr: SocketAddr) -> Result<(ServerHandle, JoinHandle<()>), Error> {
    let (done_tx, done_rx) = channel::bounded(1);
    let (shutdown_registration, shutdown_set_readiness) = Registration::new2();
    let server = Server::new(addr, shutdown_registration, done_tx)?;
    let thread = thread::spawn(move || {
        run_event_loop(server);
        info!("server done");
    });
    Ok((
        ServerHandle {
            shutdown: shutdown_set_readiness,
            done: done_rx,
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

impl Drop for ServerHandle {
    /// Gracefully shut down when the handle is dropped.
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl ServerPacket {
    fn reliable(&self) -> bool {
        match self {
            ServerPacket::Event(event) => {
                match event {
                    Event::NewPlayer {
                        ..
                    } => true,
                    Event::RemovePlayer(_) => true,
                    Event::RoundState(_) => true,
                    Event::Settings(_) => true,
                    Event::Snapshot(_) => false,
                }
            },
            ServerPacket::Handshake {
                ..
            } => true,
            ServerPacket::Ping => false,
            ServerPacket::Pong(_) => false,
        }
    }

    fn resend(&self, game: &Game) -> bool {
        match self {
            ServerPacket::Event(Event::RoundState(round)) => {
                // Only resend if the round state hasn't
                // changed again since it was sent.
                *round == game.round
            },
            ServerPacket::Event(Event::Settings(settings)) => {
                // Only resend if the round state hasn't
                // changed again since it was sent.
                &game.settings == settings
            },
            // Everything else is simple.
            _ => self.reliable(),
        }
    }
}

impl Client {
    /// Encodes a packet and possibly saves it in the reliable packet
    /// buffer.
    ///
    /// Returns the sequence number.
    fn encode(&mut self, packet: &ServerPacket) -> (Vec<u8>, u32) {
        let size = bincode::serialized_size(packet).unwrap() as usize;
        let mut data = Vec::with_capacity(size + HEADER_BYTES);
        let sequence = self.connection.send_header(&mut data);
        bincode::serialize_into(&mut data, packet).unwrap();

        if packet.reliable() {
            self.reliable.insert(sequence, packet.clone());
        }
        (data, sequence)
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
                    if let Err(err) = self.socket_readable() {
                        error!("error on reading server socket: {}", err);
                        let _ = self.done.send(Some(err));
                        return true;
                    }
                }
                if event.readiness().is_writable() {
                    if let Err(err) = self.socket_writable() {
                        error!("error on writing server socket: {}", err);
                        let _ = self.done.send(Some(err));
                        return true;
                    }
                }
            },
            TIMER => {
                while let Some(timeout) = self.timer.poll() {
                    let result = match timeout {
                        TimeoutState::SendSnapshot => self.send_snapshot(),
                        TimeoutState::Tick => self.game_tick(),
                        TimeoutState::Ping => self.send_ping(),
                        TimeoutState::LostConnection(addr) => {
                            info!("client from {} timed out", addr);
                            self.remove_client(&addr)
                        },
                    };

                    if let Err(err) = result {
                        error!("error on handling server timer event: {}", err);
                        let _ = self.done.send(Some(err));
                        return true;
                    }
                }
            },
            SHUTDOWN => {
                info!("server received shutdown from handle");
                let _ = self.done.send(None);
                return true;
            },
            _ => unreachable!(),
        }

        false
    }
}

impl Server {
    pub fn new(
        addr: SocketAddr,
        shutdown: Registration,
        done: Sender<Option<Error>>,
    ) -> Result<Server, Error> {
        let socket = UdpSocket::bind(&addr).map_err(|err| {
            Error::BindSocket {
                addr,
                err,
            }
        })?;
        let mut timer = timer::Builder::default()
            .tick_duration(Duration::from_millis(5))
            .build();
        let poll = Poll::new().map_err(Error::poll)?;
        poll.register(&socket, SOCKET, Ready::readable(), PollOpt::edge())
            .map_err(Error::poll)?;
        poll.register(&timer, TIMER, Ready::readable(), PollOpt::edge())
            .map_err(Error::poll)?;
        poll.register(&shutdown, SHUTDOWN, Ready::readable(), PollOpt::edge())
            .map_err(Error::poll)?;

        // Set timeout for the first tick. All subsequent ticks will
        // be generated from Server::send_tick.
        let send_tick = Interval::new(SNAPSHOT_RATE);
        timer.set_timeout(send_tick.interval(), TimeoutState::SendSnapshot);
        let game_tick = Interval::new(TICK_RATE);
        timer.set_timeout(game_tick.interval(), TimeoutState::Tick);
        let ping = Interval::new(PING_RATE);
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
            done,
            _shutdown: shutdown,
        })
    }

    fn socket_readable(&mut self) -> Result<(), Error> {
        // Attempt to read packets until recv_from returns WouldBlock.
        loop {
            match self.socket.recv_from(&mut self.recv_buffer) {
                Err(err) => {
                    if err.kind() != io::ErrorKind::WouldBlock {
                        error!("error receiving packet on server: {}", err);
                        // TODO figure out if any of these are non-fatal
                        return Err(Error::SocketRead(err));
                    } else {
                        break;
                    }
                },
                Ok((bytes_read, addr)) => {
                    if let Err(err) = self.on_recv(addr, bytes_read)? {
                        error!(
                            "failed to receive packet from {} ({:?}): {}",
                            addr,
                            &self.recv_buffer[..bytes_read],
                            err
                        );
                    }
                },
            }
        }

        Ok(())
    }

    fn socket_writable(&mut self) -> Result<(), Error> {
        while let Some(&(ref addr, ref packet)) = self.send_queue.front() {
            match self.socket.send_to(packet, addr) {
                Err(err) => {
                    if err.kind() != io::ErrorKind::WouldBlock {
                        error!("error sending packet to {} ({})", addr, err);
                        let (addr, _) = self.send_queue.pop_front().unwrap();
                        // Disconnect any client that errors.
                        self.remove_client(&addr)?;
                    } else {
                        break;
                    }
                },
                // Pretty sure this never happens?
                Ok(bytes_written) => {
                    if bytes_written < packet.len() {
                        error!(
                            "only wrote {} out of {} bytes for packet to {}: \
                             {:?}",
                            bytes_written,
                            packet.len(),
                            addr,
                            packet
                        )
                    }
                },
            }

            // Only pop after making sure it didn't return
            // WouldBlock.
            self.send_queue.pop_front();
        }

        if self.send_queue.is_empty() {
            // No longer care about writable events if there are no
            // more packets to send.
            self.reregister_socket(false)?;
        }
        Ok(())
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

    fn send_ping(&mut self) -> Result<(), Error> {
        let now = Instant::now();
        let (_, interval) = self.ping.next(now);
        self.timer.set_timeout(interval, TimeoutState::Ping);

        for (&addr, client) in self.clients.iter_mut() {
            let (packet, sequence) = client.encode(&ServerPacket::Ping);
            self.send_queue.push_back((addr, packet));
            client.rtt.ping(sequence, now);
        }
        self.reregister_socket(true)?;

        Ok(())
    }

    fn send_snapshot(&mut self) -> Result<(), Error> {
        // Send a snapshot to all connected clients.
        let now = Instant::now();
        let (_, interval) = self.send_tick.next(now);
        self.timer.set_timeout(interval, TimeoutState::SendSnapshot);

        let snapshot = self.game.snapshot();
        trace!("sending snapshot: {:#?}", snapshot);
        self.send_events(iter::once(Event::Snapshot(snapshot)))?;

        Ok(())
    }

    fn game_tick(&mut self) -> Result<(), Error> {
        let now = Instant::now();
        let (dt, interval) = self.game_tick.next(now);
        let dt = dt.as_float_secs() as f32;
        self.timer.set_timeout(interval, TimeoutState::Tick);

        debug!("stepping game tick (dt={})", dt);
        let events = self.game.tick(dt);
        self.send_events(events)?;

        Ok(())
    }

    fn new_client(
        &mut self,
        addr: SocketAddr,
        connection: Connection,
        cursor: Point2<f32>,
    ) -> Result<(), Error> {
        info!("new player from {}", addr);

        let timeout = self.timer.set_timeout(
            CONNECTION_TIMEOUT,
            TimeoutState::LostConnection(addr),
        );

        let (player_id, events) =
            self.game.add_player(clamp_cursor(cursor, &self.game.settings));
        self.send_events(events)?;

        // Now start processing this client.
        let client = self.clients.entry(addr).or_insert(Client {
            timeout,
            player: player_id,
            connection,
            rtt: RttEstimator::default(),
            last_input: 0,
            reliable: HashMap::new(),
        });

        // Send handshake message to the new client.
        let packet = ServerPacket::Handshake {
            id: player_id,
            settings: self.game.settings,
            players: self
                .game
                .players()
                .map(|(id, player)| (id, player.static_state().clone()))
                .collect(),
            round: self.game.round,
            round_duration: self.game.round_duration,
            snapshot: self.game.snapshot(),
        };
        let (packet, _) = client.encode(&packet);
        self.send_queue.push_back((addr, packet));
        self.reregister_socket(true)?;

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
    ) -> Result<Result<(), RecvError>, Error> {
        let mut reregister = false;

        // Reset timeout.
        if bytes_read > MAX_PACKET_SIZE {
            return Ok(Err(RecvError::PacketTooLarge(bytes_read)));
        }
        let packet = &self.recv_buffer[..bytes_read];
        trace!("got packet from {}: {:?}", addr, &packet);
        match self.clients.get_mut(&addr) {
            Some(client) => {
                // Reset timeout.
                self.timer.cancel_timeout(&client.timeout);
                client.timeout = self.timer.set_timeout(
                    CONNECTION_TIMEOUT,
                    TimeoutState::LostConnection(addr),
                );

                // Existing player.
                let (packet, sequence, acks, lost) =
                    match client.connection.decode(Cursor::new(packet)) {
                        Ok(result) => result,
                        Err(err) => return Ok(Err(err)),
                    };

                // Remove acked packets from the reliable packet
                // buffer.
                for ack in acks.iter() {
                    client.reliable.remove(&ack);
                }

                // Possibly resend any lost packets.
                for lost in lost.into_iter() {
                    if let Some(packet) = client.reliable.remove(&lost) {
                        if packet.resend(&self.game) {
                            debug!(
                                "resending lost packet to {:?}: {:?}",
                                addr, packet
                            );
                            let (packet, _) = client.encode(&packet);
                            self.send_queue.push_back((addr, packet));
                            reregister = true;
                        }
                    }
                }

                match packet {
                    ClientPacket::Input(input) => {
                        // Ignore out of order input packets.
                        if sequence > client.last_input {
                            client.last_input = sequence;
                            self.game.set_player_cursor(
                                client.player,
                                clamp_cursor(input.cursor, &self.game.settings),
                            );
                        }
                    },
                    ClientPacket::Settings(settings) => {
                        // Update the server settings.
                        self.game.settings = settings;

                        // Forward this change to the other clients.
                        let packet =
                            ServerPacket::Event(Event::Settings(settings));
                        self.broadcast_filter(&packet, |(&client_addr, _)| {
                            addr != client_addr
                        })?;
                    },
                    ClientPacket::Handshake {
                        ..
                    } => {
                        warn!(
                            "received a second handshake packet from {:?}",
                            addr
                        )
                    },
                    ClientPacket::Disconnect => {
                        self.remove_client(&addr)?;
                    },
                    ClientPacket::Ping => {
                        let (packet, _) =
                            client.encode(&ServerPacket::Pong(sequence));
                        self.send_queue.push_back((addr, packet));
                        reregister = true;
                    },
                    ClientPacket::Pong(sequence) => {
                        client.rtt.pong(sequence);
                    },
                }
            },
            None => {
                // New player.
                let mut connection = Connection::default();
                let (packet, ..) = match connection.decode(Cursor::new(packet))
                {
                    Ok(result) => result,
                    Err(err) => return Ok(Err(err)),
                };
                // Ignore non-handshake packets.
                if let ClientPacket::Handshake {
                    cursor,
                } = packet
                {
                    self.new_client(addr, connection, cursor)?;
                }
            },
        }

        if reregister {
            self.reregister_socket(true)?;
        }

        Ok(Ok(()))
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

    fn broadcast(&mut self, packet: &ServerPacket) -> Result<(), Error> {
        self.broadcast_filter(packet, |_| true)
    }

    fn broadcast_filter<F: FnMut((&SocketAddr, &Client)) -> bool>(
        &mut self,
        packet: &ServerPacket,
        mut pred: F,
    ) -> Result<(), Error> {
        if !self.clients.is_empty() {
            let data = bincode::serialize(packet).unwrap();

            for (&addr, client) in self
                .clients
                .iter_mut()
                .filter(|(addr, client)| pred((addr, &*client)))
            {
                let mut with_header =
                    Vec::with_capacity(data.len() + HEADER_BYTES);
                let sequence = client.connection.send_header(&mut with_header);
                with_header.extend_from_slice(&data);
                self.send_queue.push_back((addr, with_header));

                if packet.reliable() {
                    client.reliable.insert(sequence, packet.clone());
                }
            }

            self.reregister_socket(true)?;
        }
        Ok(())
    }
}
