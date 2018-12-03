use crate::game::{Event, GameServer, Input, PlayerId, Snapshot};
use crate::networking::client::ClientPacket;
use crate::networking::connection::{Connection, HEADER_BYTES};
use bincode;
use futures::stream;
use futures_locks::Mutex;
use failure::Error;
use serde_derive::{Serialize, Deserialize};
use log::error;
use bytes::{BytesMut,Bytes};
use futures::sync::mpsc;
use std::net::SocketAddr;
use tokio::{
    codec::BytesCodec,
    net::{UdpFramed, UdpSocket},
    prelude::*,
    timer::Interval
};
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServerPacket {
    PlayerJoined { id: PlayerId, player: Player },
    PlayerLeft(PlayerId),
    Snapshot(Snapshot),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerHandshake {
    pub id: PlayerId,
    pub elapsed: Duration,
    pub players: IntHashMap<PlayerId, PlayerClient>,
    pub snapshot: Snapshot,
}

struct Client {
    player: PlayerId,
    incoming: Sender<Bytes>,
    connection: Mutex<Connection>,
    inputs: Mutex<Inputs>,
}

#[derive(Clone, Debug)]
struct SharedState {
    outgoing: Sender<(Option<SocketAddr>, Bytes)>,
    game: Mutex<GameServer>,
    clients: Mutex<HashMap<SocketAddr, Client>>,
}

async fn handle_client(
    addr: SocketAddr,
    shared: SharedState,
    incoming: Receiver<Bytes>,
    connection: Mutex<Connection>,
    inputs: Mutex<Inputs>,
) {
    let handshake = {
        let mut game = await!(game.lock());
        let id = game.add_player();
        let elapsed = game.start.elapsed();
        ServerHandshake {
            id: id,
            elapsed: game.start.elapsed(),
            players: game
                .players
                .iter()
                .map(|id, player| (id, player.into()))
                .collect(),
        }
    };

    await!(outgoing.send((addr, handshake)));

    while let Some(mut bytes) = await!(incoming.next()) {
        let bytes_orig = bytes.clone();
        let result: Result<Error, ()> = (|| {
            await!(connection.lock()).recv_header(&mut bytes)?;
            match bincode::deserialize(&bytes)? {
                ClientPacket::Input(input) => await!(inputs.lock()).combine(input),
            }
            
            Ok(())
        });

        if let Err(err) = result {
            error!("invalid packet from {} ({}) {:?}", addr, err, bytes_orig);
        }
    }
}

fn spawn_server(addr: SocketAddr) -> Result<(), Error> {
    let mut socket = UdpSocket::bind(&addr)?;

    let clients = Mutex::new(HashMap::new());
    let (sock_read, sock_write) = UdpFramed::new(socket, BytesCodec::new()).split();
    let (tx_outgoing, rx_outgoing) = mpsc::channel();
    let shared = SharedState {
        outgoing: tx_outgoing,
        game: Mutex::new(GameServer::default()),
        clients: Mutex::new(HashMap::new()),
    };

    // Update ticks.
    tokio::spawn_async({
        let shared = shared.clone();
        // 60hz
        let mut ticks = Interval::new_interval(Duration::from_float_secs(1.0 / 60.0));

        async move {
            while let Some(_) = await!(ticks.next()) {
                let mut clients = await!(clients.lock());
                let mut game = await!(game.lock());

                for client in clients.values() {
                    let mut input = await!(client.inputs().lock());
                    game.players[&client.player].input(input);
                    input.clear();
                }
            }
        }
    });

    // Send snapshots.
    tokio::spawn_async({
        let shared = shared.clone();
        // 30hz
        let mut ticks = Interval::new_interval(Duration::from_float_secs(1.0 / 30.0));

        async move {
            while let Some(_) = await!(ticks.next()) {
                let snapshot = await!(game.lock()).snapshot();
                let packet = ServerPacket::Snapshot(snapshot);
                shared.outgoing.send(bincode::serialize());
            }
        }
    });

    // Read side.
    tokio::spawn_async({
        let shared = shared.clone();

        let socket = socket_read;
        async move {
            while let Some(result) = await!(socket.next()) {
                let (mut bytes, addr) = result?;

                let mut clients = await!(shared.clients.lock());
                match clients.get(&addr) {
                    Some(client) => await!(client.incoming.send(bytes)),
                    None => {
                        // New client has connected.
                        let (tx_incoming, rx_incoming) = mpsc::channel();
                        let player = await!(game.lock()).add_player();
                        await!(shared.outgoing.send((None, bytes)));
                        let connection = Mutex::new(Connection::new());
                        let inputs = Mutex::new(Input::default());
                        let client = Client {
                            player,
                            incoming: tx_incoming,
                            connection: connection.clone(),
                            inputs: inputs.clone(),
                        };
                        clients.insert(addr, rx_incoming);
                        tokio::spawn_async(handle_client(
                            addr,
                            shared.clone(),
                            rx_incoming,
                            connection,
                            inputs
                        ));
                    }
                }
            }
        }
    });

    // Write side.
    tokio::spawn_async({
        let shared = shared.clone();
        let socket = socket_write;

        async move {
            while let Some(packet) = await!(rx_outgoing.next()) {
                let (bytes, addr) = result?;

                match addr {
                    Some(addr) => await!(socket.send((bytes, addr))),
                    None => {
                        // Broadcast
                        let packets = await!(clients.lock())
                            .values()
                            .map(|client| {
                                let mut header = BytesMut::with_capacity(HEADER_BYTES);
                                await!(client.connection.lock()).send_header(&mut header);
                                header.freeze().chain(bytes.clone())
                            }(bytes.clone(), addr));
                        await!(socket.send_all(stream::futures_unordered(packets)));
                    }
                }
            }
        }
    });

    Ok(())
}
