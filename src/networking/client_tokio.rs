use crate::double_buffer::DoubleBuffer;
use crate::game::{GameClient, Input};
use crate::networking::connection::{Connection, HEADER_BYTES};
use crate::networking::server::{ServerHandshake, ServerPacket};
use bincode;
use bytes::{Bytes, BytesMut};
use failure::{Error, Fail};
use futures::sync::{mpsc, oneshot, BiLock};
use log::error;
use serde_derive::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tokio::{
    codec::BytesCodec,
    net::{UdpFramed, UdpSocket},
    prelude::*,
    timer::Interval,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ClientPacket {
    Input(Input),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientHandshake;

pub fn connect_client(addr: SocketAddr) -> oneshot::Receiver<Result<GameClient, Option<Error>>> {
    let (tx, rx) = oneshot::channel();
    tokio::run_async(
        async move {
            let connect_timeout = Duration::from_secs(4.0);
            tx.send(await!(spawn_client(addr).timeout(connect_timeout)));
        },
    );
}

async fn spawn_client(addr: SocketAddr) -> Result<GameClient, Error> {
    // TODO: do I need to retry sending the handshake packet?
    let mut socket = UdpSocket::bind(&"0.0.0.0:0".parse().unwrap())?;
    socket.connect(addr)?;

    let handshake = ClientHandshake;
    let payload = bincode::serialize(&handshake).unwrap();
    tokio::spawn(socket.send_dgram(&handshake));

    let mut packet = Vec::new();
    await!(socket.recv_dgram(&mut packet))?;
    let handshake = bincode::deserialize(&packet).unwrap();

    let (events_net, events_game) = mpsc::channel(16);
    let (input_game, input_net) = BiLock::new(Input::default());
    let game = GameClient {
        start: Instant::now() - handshake.elapsed,
        snapshots: DoubleBuffer::new(handshake.snapshot),
        events: events_game,
        input: input_game,
    };

    let (conn_read, conn_write) = BiLock::new(Connection::new());
    let (sock_read, sock_write) = UdpFramed::new(socket, BytesCodec::new()).split();

    // Read side.
    tokio::spawn_async(
        async move {
            let (socket, connection) = (sock_read, conn_read);

            while let Some(result) = await!(socket.next()) {
                let (mut bytes, _) = result?;
                await!(connection.lock())?.recv_header(&mut bytes)?;

                match bincode::deserialize(&bytes)? {
                    ServerPacket::Event(event) => await!(events_net.send(event)),
                }
            }
        },
    );

    // Write side.
    tokio::spawn_async(
        async move {
            let (socket, connection) = (sock_write, conn_write);

            // 10hz
            let mut ticks = Interval::new_interval(Duration::from_float_secs(1.0 / 10.0));
            let mut bytes = Bytes::new();

            while let Some(_) = await!(ticks.next()) {
                let packet = ClientPacket::Input(await!(input_net.lock()).clone());

                let packet_bytes = bincode::serialized_size(&packet);
                let bytes_needed = HEADER_BYTES + packet_bytes;

                // Equivalent to `to_mut`, except that cloning isn't
                // needed, and exact capacity is known.
                let mut bytes_mut = match bytes.try_mut() {
                    Ok(mut bytes_mut) => {
                        bytes_mut.clear();
                        bytes_mut
                    }
                    Err(_) => BytesMut::with_capacity(bytes_needed),
                };

                await!(connection.lock())?.send_header(&mut bytes_mut)?;
                bincode::serialize_into(&mut bytes_mut, &packet)?;

                let bytes = bytes_mut.freeze().clone();
                await!(socket.send((bytes, addr)));
            }
        },
    );

    Ok(game)
}
