use bincode;
use failure::{Backtrace, Fail};
use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

pub mod client;
pub mod connection;
pub mod event_loop;
pub mod server;
pub mod tick;

/// MTU will probably never be bigger than this, so if a received
/// packet is bigger, there are probably other problems.
pub const MAX_PACKET_SIZE: usize = 4096;

/// Rate at which both the client and the server send out pings.
pub const PING_RATE: Duration = Duration::from_millis(500);

/// Rate at which the server sends snapshots.
pub const SNAPSHOT_RATE: Duration = Duration::from_millis(30);

/// Seconds to wait before marking a connection as timed out.
pub const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// System to estimate rtt for a connection by periodically sending
/// pings and recording the time until a response is received.
#[derive(Default, Debug)]
pub struct RttEstimator {
    /// Sequence id and timestamp of the last sent ping.
    ///
    /// This gets set back to `None` once a pong is received.
    last_ping: Option<(u32, Instant)>,
    rtt: Option<f32>,
}

impl RttEstimator {
    /// Gets the estimated RTT, or `None` if there have been no
    /// samples yet.
    pub fn rtt(&self) -> Option<f32> {
        self.rtt
    }

    /// Record a sent ping.
    pub fn ping(&mut self, sequence: u32, now: Instant) {
        self.last_ping = Some((sequence, now));
    }

    /// Record a pong response.
    pub fn pong(&mut self, sequence: u32) {
        if let Some((expected, time)) = self.last_ping {
            if sequence == expected {
                let sample = time.elapsed().as_secs_f32();
                self.rtt = Some(match self.rtt {
                    Some(rtt) => 0.875 * rtt + 0.125 * sample,
                    None => sample,
                });
                self.last_ping = None;
            }
        }
    }
}

/// Non-fatal errors that occur on receiving a packet.
///
/// These should be logged, but generally do not end the connection.
#[derive(Fail, Debug)]
pub enum RecvError {
    #[fail(display = "received packet that was too large ({} bytes)", _0)]
    PacketTooLarge(usize),
    #[fail(display = "reading packet header failed: {} {}", _0, _1)]
    HeaderRead(io::Error, Backtrace),
    #[fail(display = "deserializing packet payload failed: {} {}", _0, _1)]
    Deserialize(bincode::Error, Backtrace),
}

/// (Mostly) fatal errors that should kill either the networking event
/// loop, or the particular connection in question.
#[derive(Fail, Debug)]
pub enum Error {
    #[fail(display = "connection timed out")]
    TimedOut,
    #[fail(display = "poll error: {} {}", _0, _1)]
    Poll(#[cause] io::Error, Backtrace),
    #[fail(display = "binding socket to {:?} failed: {}", addr, err)]
    BindSocket {
        addr: SocketAddr,
        #[cause]
        err: io::Error,
    },
    #[fail(display = "connecting socket to {:?} failed: {}", addr, err)]
    ConnectSocket {
        addr: SocketAddr,
        #[cause]
        err: io::Error,
    },
    #[fail(display = "socket write failed: {}", _0)]
    SocketWrite(io::Error),
    #[fail(display = "socket read failed: {}", _0)]
    SocketRead(io::Error),
}

impl Error {
    pub fn poll(err: io::Error) -> Error {
        Error::Poll(err, Backtrace::new())
    }
}

impl RecvError {
    pub fn header_read(err: io::Error) -> RecvError {
        RecvError::HeaderRead(err, Backtrace::new())
    }

    pub fn deserialize(err: bincode::Error) -> RecvError {
        RecvError::Deserialize(err, Backtrace::new())
    }
}
