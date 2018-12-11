use crate::double_buffer::DoubleBuffer;
use crate::game::GameClient;
use crate::networking::connection::{Connection, HEADER_BYTES};
use crate::networking::event_loop::{run_event_loop, EventHandler};
use crate::networking::server::{ServerHandshake, ServerPacket};
use crate::networking::tick::Interval;
use crate::networking::{Error, MAX_PACKET_SIZE};
use cgmath::Point2;
use log::{error, info, trace, warn};
use mio::net::UdpSocket;
use mio::{Event, Poll, PollOpt, Ready, Token};
use mio_extras::channel::{self as mio_channel, Receiver as MioReceiver, Sender as MioSender};
use mio_extras::timer::{self, Timeout, Timer};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{self, Cursor};
use std::net::SocketAddr;
use std::sync::mpsc::{
    self as std_channel, Receiver as StdReceiver, Sender as StdSender, TryRecvError,
};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const SOCKET: Token = Token(0);
const TIMER: Token = Token(1);
const SHUTDOWN: Token = Token(2);

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum TimeoutState {
    Tick,
    Connect,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ClientPacket {
    Input { position: Point2<f32> },
    Disconnect,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientHandshake;

pub enum ClientState {
    Connecting {
        done: StdSender<Result<Arc<GameClient>, Error>>,
        timeout: Timeout,
    },
    Connected {
        tick: Interval,
        snapshot_seq_ids: DoubleBuffer<u32>,
        game: Arc<GameClient>,
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
    shutdown: MioReceiver<()>,
    /// Marks after `shutdown` has been received, to shutdown when the
    /// `send_queue` is empty.
    needs_shutdown: bool,
}

/// Client handle used while connecting to a sever.
pub struct ClientHandle {
    shutdown: MioSender<()>,
}

pub struct ConnectingHandle {
    done: StdReceiver<Result<Arc<GameClient>, Error>>,
}

pub fn connect(addr: SocketAddr) -> Result<(ClientHandle, ConnectingHandle), Error> {
    let (done_tx, done_rx) = std_channel::channel();
    let (shutdown_tx, shutdown_rx) = mio_channel::channel();
    let mut client = Client::new(addr, done_tx, shutdown_rx)?;
    thread::spawn(move || {
        run_event_loop(&mut client);
    });
    Ok((
        ClientHandle {
            shutdown: shutdown_tx,
        },
        ConnectingHandle { done: done_rx },
    ))
}

impl ClientHandle {
    /// Signals the client thread to shutdown.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(());
    }
}

impl ConnectingHandle {
    /// Gets the connection result, if connection finished.
    pub fn done(&mut self) -> Option<Result<Arc<GameClient>, Error>> {
        match self.done.try_recv() {
            Ok(done) => Some(done),
            Err(TryRecvError::Empty) => None,
            // Disconnected also means it never finished connecting,
            // but maybe we should handle this a different way.
            Err(TryRecvError::Disconnected) => None,
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
            }
            TIMER => {
                while let Some(timeout) = self.timer.poll() {
                    match timeout {
                        TimeoutState::Tick => {
                            if let Err(err) = self.send_tick() {
                                error!("error sending tick packet: {}", err);
                            }
                        }
                        TimeoutState::Connect => {
                            if let ClientState::Connecting { ref mut done, .. } = self.state {
                                let _ = done.send(Err(Error::TimedOut));
                                info!("client connection timed out");
                                return true;
                            }
                        }
                    }
                }
            }
            SHUTDOWN => match self.shutdown.try_recv() {
                Ok(()) | Err(TryRecvError::Disconnected) => {
                    info!("client started shutdown");
                    if let Err(err) = self.send(&ClientPacket::Disconnect) {
                        error!("failed to send disconnect packet: {}", err);
                        // If this errored, just shut down immediately.
                        return true;
                    }
                    self.needs_shutdown = true;
                }
                Err(TryRecvError::Empty) => (),
            },
            Token(_) => unreachable!(),
        }

        false
    }
}

impl Client {
    pub fn new(
        addr: SocketAddr,
        done: StdSender<Result<Arc<GameClient>, Error>>,
        shutdown: MioReceiver<()>,
    ) -> Result<Client, Error> {
        let socket = UdpSocket::bind(&"0.0.0.0:0".parse().unwrap()).map_err(Error::bind_socket)?;
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

        let timeout = timer.set_timeout(Duration::from_secs(5), TimeoutState::Connect);

        let mut client = Client {
            socket,
            timer,
            recv_buffer: [0; MAX_PACKET_SIZE],
            send_queue: VecDeque::new(),
            poll,
            connection: Connection::default(),
            state: ClientState::Connecting { done, timeout },
            shutdown,
            needs_shutdown: false,
        };

        // Send handshake
        client.send(&ClientHandshake)?;

        Ok(client)
    }

    pub fn socket_readable(&mut self) {
        loop {
            match self.socket.recv(&mut self.recv_buffer) {
                Ok(bytes_read) => {
                    if let Err(err) = self.on_recv(bytes_read) {
                        error!("{}", err);
                    }
                }
                Err(err) => {
                    if err.kind() != io::ErrorKind::WouldBlock {
                        error!("error receiving packet on client: {}", err);
                    } else {
                        break;
                    }
                }
            }
        }
    }

    pub fn socket_writable(&mut self) {
        while let Some(packet) = self.send_queue.pop_front() {
            match self.socket.send(&packet) {
                Err(err) => {
                    if err.kind() != io::ErrorKind::WouldBlock {
                        error!("error sending packet from client ({:?}): {}", &packet, err);
                    } else {
                        break;
                    }
                }
                // Pretty sure this never happens?
                Ok(bytes_written) => {
                    if bytes_written < packet.len() {
                        error!(
                            "Only wrote {} out of {} bytes for client packet: {:?}",
                            bytes_written,
                            packet.len(),
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
        match self.state {
            ClientState::Connected {
                ref mut tick,
                ref game,
                ..
            } => {
                let now = Instant::now();
                let (_, interval) = tick.next(now);
                self.timer.set_timeout(interval, TimeoutState::Tick);

                let packet = ClientPacket::Input {
                    position: game.input.position(),
                };
                trace!("sending tick packet to server: {:?}", packet);
                self.send(&packet)?;
            }
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
            } => {
                // Assumed to be a handshake packet.
                let mut cursor = Cursor::new(packet);
                let sequence = self.connection.recv_header(&mut cursor)?;
                let handshake: ServerHandshake =
                    bincode::deserialize_from(&mut cursor).map_err(Error::deserialize)?;
                let game = Arc::new(GameClient::new(
                    handshake.players,
                    handshake.snapshot,
                    handshake.id,
                ));
                let tick = Interval::new(Duration::from_float_secs(1.0 / 30.0));
                // Start the timer for sending input ticks.
                self.timer.cancel_timeout(timeout);
                self.timer.set_timeout(tick.interval(), TimeoutState::Tick);

                // Signal the main thread that connection finished.
                done.send(Ok(game.clone())).unwrap();

                info!("completed connection to server");
                // Transition to connected state.
                Some(ClientState::Connected {
                    game,
                    tick,
                    snapshot_seq_ids: DoubleBuffer::new(sequence),
                })
            }

            ClientState::Connected {
                ref mut game,
                ref mut snapshot_seq_ids,
                ..
            } => {
                let mut read = Cursor::new(packet);
                let sequence = self.connection.recv_header(&mut read)?;
                let packet = bincode::deserialize_from(&mut read).map_err(Error::deserialize)?;

                match packet {
                    ServerPacket::Snapshot(snapshot) => {
                        trace!("got snapshot from server");
                        if sequence > *snapshot_seq_ids.get() {
                            // Things are normal, rotate the buffers
                            // as expected.
                            game.insert_snapshot(snapshot, true);
                            snapshot_seq_ids.insert(sequence);
                            snapshot_seq_ids.swap();
                        } else if sequence > *snapshot_seq_ids.get_old() {
                            // This snapshot belongs in between the
                            // current ones, so just replace old.
                            game.insert_snapshot(snapshot, false);
                            snapshot_seq_ids.insert(sequence);
                        }
                        // Otherwise it's really old and we don't
                        // care.
                    }
                    ServerPacket::PlayerJoined { id, player } => {
                        info!("new player joined: {}", id);
                        let mut players = game.players.lock();
                        players.insert(id, player);
                    }
                    ServerPacket::PlayerLeft(id) => {
                        info!("player {} left", id);
                        let mut players = game.players.lock();
                        if players.remove(&id).is_none() {
                            warn!("server says player {} left, but client didn't register that player at all", id);
                        }
                    }
                }

                None
            }
        };

        if let Some(transition) = transition {
            self.state = transition;
        }

        Ok(())
    }

    fn send<P: Serialize>(&mut self, contents: &P) -> Result<(), Error> {
        // Don't send any additional packets while shutting down.
        if self.needs_shutdown {
            return Ok(());
        }

        let size = bincode::serialized_size(contents).map_err(Error::serialize)? as usize;
        let mut packet = Vec::with_capacity(size + HEADER_BYTES);
        self.connection.send_header(&mut packet)?;
        bincode::serialize_into(&mut packet, contents).map_err(Error::serialize)?;
        self.send_queue.push_back(packet);
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
