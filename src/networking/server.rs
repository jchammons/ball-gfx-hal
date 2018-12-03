use crate::game::{GameServer, PlayerClient, PlayerId, Snapshot};
use crate::networking::client::ClientPacket;
use crate::networking::connection::{Connection, HEADER_BYTES};
use crate::networking::event_loop::{run_event_loop, EventHandler};
use crate::networking::tick::Interval;
use crate::networking::{Error, MAX_PACKET_SIZE};
use int_hash::IntHashMap;
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
use std::thread;
use std::time::{Duration, Instant};

const SOCKET: Token = Token(0);
const TIMER: Token = Token(1);
const SHUTDOWN: Token = Token(2);

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum TimeoutState {
    Tick,
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
    pub players: IntHashMap<PlayerId, PlayerClient>,
    pub snapshot: Snapshot,
}

struct Client {
    player: PlayerId,
    connection: Connection,
}

pub struct Server {
    socket: UdpSocket,
    timer: Timer<TimeoutState>,
    recv_buffer: [u8; MAX_PACKET_SIZE],
    send_queue: VecDeque<(SocketAddr, Vec<u8>)>,
    clients: HashMap<SocketAddr, Client>,
    game: GameServer,
    tick: Interval,
    poll: Poll,
    shutdown: Receiver<()>,
}

pub struct ServerHandle {
    shutdown: Sender<()>,
}

impl Client {
    fn new(player: PlayerId) -> Client {
        // Ignore the handshake for now.
        Client {
            player,
            connection: Connection::new(),
        }
    }

    fn encode<P: Serialize>(&mut self, packet: &P) -> Result<Vec<u8>, Error> {
        let size = bincode::serialized_size(packet).map_err(Error::serialize)?;
        let mut buf = Vec::with_capacity(HEADER_BYTES + size as usize);
        self.connection.send_header(&mut buf)?;
        bincode::serialize_into(&mut buf, packet).map_err(Error::serialize)?;
        Ok(buf)
    }

    fn decode<'a, P: DeserializeOwned>(&mut self, packet: &'a [u8]) -> Result<P, Error> {
        let mut read = Cursor::new(packet);
        self.connection.recv_header(&mut read)?;
        let packet = bincode::deserialize_from(&mut read).map_err(Error::deserialize)?;
        Ok(packet)
    }
}

/// Launches a server bound to a particular address.
pub fn host(addr: SocketAddr) -> Result<ServerHandle, Error> {
    let (shutdown_tx, shutdown_rx) = channel::channel();
    let mut server = Server::new(&addr, shutdown_rx)?;
    thread::spawn(move || {
        run_event_loop(&mut server);
    });
    Ok(ServerHandle {
        shutdown: shutdown_tx,
    })
}

impl ServerHandle {
    /// Attmepts to signal the associated server to shutdown.
    pub fn shutdown(&mut self) {
        let _ = self.shutdown.send(());
    }
}

// Gracefully shut down when the handle is dropped.
impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown();
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
                        TimeoutState::Tick => {
                            if let Err(err) = self.send_tick() {
                                error!("error when sending tick: {}", err);
                            }
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
        let tick = Interval::new(Duration::from_float_secs(1.0 / 30.0));
        timer.set_timeout(tick.interval(), TimeoutState::Tick);

        Ok(Server {
            socket,
            timer,
            recv_buffer: [0; MAX_PACKET_SIZE],
            send_queue: VecDeque::new(),
            clients: HashMap::new(),
            game: GameServer::new(),
            tick,
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
        while let Some((addr, packet)) = self.send_queue.pop_front() {
            match self.socket.send_to(&packet, &addr) {
                Err(err) => {
                    if err.kind() != io::ErrorKind::WouldBlock {
                        error!("error sending packet to {} ({}): {:?}", addr, err, &packet);
                    }
                    break;
                }
                // Pretty sure this never happens?
                Ok(bytes_written) => {
                    if bytes_written < packet.len() {
                        error!(
                            "Only wrote {} out of {} bytes for packet to {}: {:?}",
                            bytes_written,
                            packet.len(),
                            addr,
                            &packet
                        )
                    }
                }
            }
        }

        if self.send_queue.is_empty() {
            // No longer care about writable events if there are no
            // more packets to send.
            if let Err(err) = self
                .poll
                .reregister(&self.socket, SOCKET, Ready::readable(), PollOpt::edge())
                .map_err(Error::poll_register)
            {
                error!("{}", err);
            }
        }
    }

    fn send_tick(&mut self) -> Result<(), Error> {
        // Send a snapshot to all connected clients.
        let now = Instant::now();
        let interval = self.tick.next(now);
        self.timer.set_timeout(interval, TimeoutState::Tick);

        debug!("sending snapshot to clients");
        let packet = ServerPacket::Snapshot(self.game.snapshot());
        let packet = bincode::serialize(&packet).map_err(Error::serialize)?;
        self.broadcast(&packet)?;

        Ok(())
    }

    fn new_client(&mut self, addr: SocketAddr) -> Result<(), Error> {
        info!("new player from {}", addr);
        let player = self.game.add_player();
        let client = Client::new(player);

        // Send handshake message to the new client.
        let handshake = ServerHandshake {
            id: player,
            players: self
                .game
                .players
                .iter()
                .map(|(&id, player)| (id, player.as_client()))
                .collect(),
            snapshot: self.game.snapshot(),
        };
        let packet = bincode::serialize(&handshake).map_err(Error::serialize)?;
        self.send_to(addr, packet)?;

        // Broadcast join message to the other clients.
        let packet = ServerPacket::PlayerJoined {
            id: player,
            player: self.game.players[&player].as_client(),
        };
        let packet = bincode::serialize(&packet).map_err(Error::serialize)?;
        self.broadcast(&packet)?;

        // Now start processing this client.
        self.clients.insert(addr, client);

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
                    ClientPacket::Input(input) => {
                        if let Some(player) = self.game.players.get_mut(&client.player) {
                            player.input(&input);
                        }
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

    fn send_to(&mut self, addr: SocketAddr, packet: Vec<u8>) -> Result<(), Error> {
        self.send_queue.push_back((addr, packet));
        self.poll
            .reregister(
                &self.socket,
                SOCKET,
                Ready::readable() | Ready::writable(),
                PollOpt::edge(),
            )
            .map_err(Error::poll_register)?;

        Ok(())
    }

    fn broadcast(&mut self, packet: &[u8]) -> Result<(), Error> {
        for (addr, client) in self.clients.iter_mut() {
            let mut with_header = Vec::with_capacity(HEADER_BYTES + packet.len());
            with_header.extend((0..HEADER_BYTES).map(|_| 0).chain(packet.iter().cloned()));
            client
                .connection
                .send_header(&mut with_header[0..HEADER_BYTES])?;
            self.send_queue.push_back((*addr, with_header));
        }

        // Only reregister once, at the very end.
        self.poll
            .reregister(
                &self.socket,
                SOCKET,
                Ready::readable() | Ready::writable(),
                PollOpt::edge(),
            )
            .map_err(Error::poll_register)?;

        Ok(())
    }
}
