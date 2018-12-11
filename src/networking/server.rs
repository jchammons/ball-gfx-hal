use crate::game::{GameServer, PlayerClient, PlayerId, Snapshot};
use crate::networking::client::ClientPacket;
use crate::networking::connection::{Connection, HEADER_BYTES};
use crate::networking::event_loop::{run_event_loop, EventHandler};
use crate::networking::tick::Interval;
use crate::networking::{Error, MAX_PACKET_SIZE};
use log::{debug, error, info, trace};
use mio::net::UdpSocket;
use mio::{Event, Poll, PollOpt, Ready, Token};
use mio_extras::channel::{self, Receiver, Sender};
use mio_extras::timer::{self, Timer};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::mpsc::TryRecvError;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const SOCKET: Token = Token(0);
const TIMER: Token = Token(1);
const SHUTDOWN: Token = Token(2);

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum TimeoutState {
    SendTick,
    GameTick,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServerPacket {
    PlayerJoined { id: PlayerId, player: PlayerClient },
    PlayerLeft(PlayerId),
    Snapshot(Snapshot),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerHandshake {
    pub id: PlayerId,
    pub players: HashMap<PlayerId, PlayerClient>,
    pub snapshot: Snapshot,
}

struct Client {
    player: PlayerId,
    connection: Connection,
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
    game: GameServer,
    send_tick: Interval,
    game_tick: Interval,
    poll: Poll,
    shutdown: Receiver<()>,
}

pub struct ServerHandle {
    pub shutdown: Sender<()>,
}

impl Client {
    fn new(player: PlayerId) -> Client {
        // Ignore the handshake for now.
        Client {
            player,
            connection: Connection::default(),
        }
    }

    fn decode<'a, P: DeserializeOwned>(&mut self, packet: &'a [u8]) -> Result<P, Error> {
        let mut read = Cursor::new(packet);
        self.connection.recv_header(&mut read)?;
        let packet = bincode::deserialize_from(&mut read).map_err(Error::deserialize)?;
        Ok(packet)
    }
}

/// Launches a server bound to a particular address.
pub fn host(addr: SocketAddr) -> Result<(ServerHandle, JoinHandle<()>), Error> {
    let (shutdown_tx, shutdown_rx) = channel::channel();
    let mut server = Server::new(&addr, shutdown_rx)?;
    let thread = thread::spawn(move || {
        run_event_loop(&mut server);
    });
    Ok((
        ServerHandle {
            shutdown: shutdown_tx,
        },
        thread,
    ))
}

impl ServerHandle {
    /// Attmepts to signal the associated server to shutdown.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(());
    }
}

// Gracefully shut down when the handle is dropped.
impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Packet {
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
        let size = bincode::serialized_size(contents).map_err(Error::serialize)? as usize;
        let mut packet = Vec::with_capacity(size + HEADER_BYTES);
        let client_state = clients_state.get_mut(&client).unwrap();
        client_state.connection.send_header(&mut packet)?;
        bincode::serialize_into(&mut packet, contents).map_err(Error::serialize)?;

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
                let connection = &mut clients_state.get_mut(&client).unwrap().connection;
                let cursor = Cursor::new(&mut self.packet[..HEADER_BYTES]);
                connection.send_header(cursor)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

impl EventHandler for Server {
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
            }
            TIMER => {
                while let Some(timeout) = self.timer.poll() {
                    match timeout {
                        TimeoutState::SendTick => {
                            if let Err(err) = self.send_tick() {
                                error!("error when sending tick: {}", err);
                            }
                        }
                        TimeoutState::GameTick => {
                            self.game_tick();
                        }
                    }
                }
            }
            SHUTDOWN => match self.shutdown.try_recv() {
                Ok(()) => {
                    info!("server received shutdown from handle");
                    return true;
                }
                Err(TryRecvError::Disconnected) => {
                    error!("server handle has disconnected without sending shutdown");
                    return true;
                }
                Err(TryRecvError::Empty) => (),
            },
            _ => unreachable!(),
        }

        false
    }
}

impl Server {
    pub fn new(addr: &SocketAddr, shutdown: Receiver<()>) -> Result<Server, Error> {
        let socket = UdpSocket::bind(addr).map_err(Error::bind_socket)?;
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

        // Set timeout for the first tick. All subsequent ticks will
        // be generated from Server::send_tick.
        let send_tick = Interval::new(Duration::from_float_secs(1.0 / 30.0)); // 30hz
        timer.set_timeout(send_tick.interval(), TimeoutState::SendTick);
        let game_tick = Interval::new(Duration::from_float_secs(1.0 / 60.0)); // 60hz
        timer.set_timeout(game_tick.interval(), TimeoutState::GameTick);

        Ok(Server {
            socket,
            timer,
            recv_buffer: [0; MAX_PACKET_SIZE],
            send_queue: VecDeque::new(),
            clients: HashMap::new(),
            game: GameServer::default(),
            send_tick,
            game_tick,
            poll,
            shutdown,
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
                }
                Ok((bytes_read, addr)) => {
                    if let Err(err) = self.on_recv(addr, bytes_read) {
                        error!("error receiving packet from {}: {}", addr, err);
                    }
                }
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
                }
                // Pretty sure this never happens?
                Ok(bytes_written) => {
                    if bytes_written < packet.packet.len() {
                        error!(
                            "only wrote {} out of {} bytes for packet to {}: {:?}",
                            bytes_written,
                            packet.packet.len(),
                            &packet.client,
                            &packet.packet
                        )
                    }
                }
            }

            // If it got here, the packet must have actually been sent.
            match packet.next_packet(&mut self.clients) {
                Ok(true) => {
                    // Continue as usual.
                }
                Ok(false) => {
                    self.send_queue.pop_front().unwrap();
                }
                Err(err) => {
                    error!("error writing header for packet: {}", err);
                }
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

    fn send_tick(&mut self) -> Result<(), Error> {
        // Send a snapshot to all connected clients.
        let now = Instant::now();
        let (_, interval) = self.send_tick.next(now);
        self.timer.set_timeout(interval, TimeoutState::SendTick);

        debug!("sending snapshot to clients");
        let packet = ServerPacket::Snapshot(self.game.snapshot());
        self.broadcast(&packet)?;

        Ok(())
    }

    fn game_tick(&mut self) {
        let now = Instant::now();
        let (dt, interval) = self.game_tick.next(now);
        let mut dt = dt.as_float_secs() as f32;
        self.timer.set_timeout(interval, TimeoutState::GameTick);

        debug!("stepping game tick (dt={})", dt);
        // Make sure that the simulation is never stepped faster than
        // 60hz, even if dt>1/60 sec.
        while dt > 1.0 / 60.0 {
            self.game.tick(1.0 / 60.0);
            dt -= 1.0 / 60.0;
        }
        self.game.tick(dt);
    }

    fn new_client(&mut self, addr: SocketAddr) -> Result<(), Error> {
        info!("new player from {}", addr);
        let player = self.game.add_player();
        // Now start processing this client.
        self.clients.insert(addr, Client::new(player));

        // Send handshake message to the new client.
        let packet = ServerHandshake {
            id: player,
            players: self
                .game
                .players
                .iter()
                .map(|(&id, player)| (id, player.as_client()))
                .collect(),
            snapshot: self.game.snapshot(),
        };
        self.send_to(addr, &packet)?;

        // Broadcast join message to the every other client.
        let packet = ServerPacket::PlayerJoined {
            id: player,
            player: self.game.players[&player].as_client(),
        };
        self.broadcast_filter(|client_addr, _| client_addr != &addr, &packet)?;

        Ok(())
    }

    fn remove_client(&mut self, addr: &SocketAddr) -> Result<(), Error> {
        let client = self.clients.remove(addr).unwrap();
        info!("player {} from {} left", client.player, addr);
        self.game.remove_player(client.player);

        // Send disconnect message to rest of clients.
        let packet = ServerPacket::PlayerLeft(client.player);
        self.broadcast(&packet)?;

        Ok(())
    }

    fn on_recv(&mut self, addr: SocketAddr, bytes_read: usize) -> Result<(), Error> {
        if bytes_read > MAX_PACKET_SIZE {
            return Err(Error::PacketTooLarge(bytes_read));
        }
        let packet = &self.recv_buffer[0..bytes_read];
        trace!("got packet from {}: {:?}", addr, &packet);
        match self.clients.get_mut(&addr) {
            Some(client) => {
                // Existing player.
                match client.decode(packet)? {
                    ClientPacket::Input { position } => {
                        if let Some(player) = self.game.players.get_mut(&client.player) {
                            player.position = position;
                        }
                    }
                    ClientPacket::Disconnect => {
                        self.remove_client(&addr)?;
                    }
                }
            }
            None => {
                // New player.
                self.new_client(addr)?;
            }
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

    fn send_to<P: Serialize>(&mut self, addr: SocketAddr, packet: &P) -> Result<(), Error> {
        self.send_queue
            .push_back(Packet::new(Some(addr), packet, &mut self.clients)?.unwrap());
        self.reregister_socket(true)?;
        Ok(())
    }

    fn broadcast<P: Serialize>(&mut self, packet: &P) -> Result<(), Error> {
        self.send_queue.push_back(
            Packet::new(
                self.clients.keys().cloned().collect::<Vec<_>>(),
                packet,
                &mut self.clients,
            )?
            .unwrap(),
        );
        self.reregister_socket(true)?;
        Ok(())
    }

    fn broadcast_filter<P: Serialize, F: Fn(&SocketAddr, &Client) -> bool>(
        &mut self,
        filter: F,
        packet: &P,
    ) -> Result<(), Error> {
        let packet = Packet::new(
            self.clients
                .iter()
                .filter_map(|(addr, client)| {
                    if filter(addr, client) {
                        Some(*addr)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>(),
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
