use failure::{Backtrace, Fail};
use std::io;
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
                let sample = time.elapsed().as_float_secs() as f32;
                self.rtt = Some(match self.rtt {
                    Some(rtt) => 0.875 * rtt + 0.125 * sample,
                    None => sample,
                });
                self.last_ping = None;
            }
        }
    }
}

#[derive(Fail, Debug)]
pub enum Error {
    #[fail(display = "connection timed out")]
    TimedOut,
    #[fail(
        display = "got a packet of size {}, which is bigger than \
                   MAX_PACKET_SIZE",
        _0
    )]
    PacketTooLarge(usize),
    #[fail(display = "reading header failed: {} {}", _0, _1)]
    HeaderRead(#[cause] io::Error, Backtrace),
    #[fail(display = "writing header failed: {} {}", _0, _1)]
    HeaderWrite(#[cause] io::Error, Backtrace),
    #[fail(display = "deserializing packet failed: {} {}", _0, _1)]
    Deserialize(#[cause] bincode::Error, Backtrace),
    #[fail(display = "serializing packet failed: {} {}", _0, _1)]
    Serialize(#[cause] bincode::Error, Backtrace),
    #[fail(display = "binding server socket failed: {} {}", _0, _1)]
    BindSocket(#[cause] io::Error, Backtrace),
    #[fail(display = "connecting to server socket failed: {} {}", _0, _1)]
    ConnectSocket(#[cause] io::Error, Backtrace),
    #[fail(display = "registering poll event failed: {} {}", _0, _1)]
    PollRegister(#[cause] io::Error, Backtrace),
    #[fail(display = "initializing poll failed: {} {}", _0, _1)]
    PollInit(#[cause] io::Error, Backtrace),
    #[fail(display = "connection is shutting down")]
    ShuttingDown,
}

impl Error {
    pub fn header_read(err: io::Error) -> Error {
        Error::HeaderRead(err, Backtrace::new())
    }

    pub fn header_write(err: io::Error) -> Error {
        Error::HeaderWrite(err, Backtrace::new())
    }

    pub fn deserialize(err: bincode::Error) -> Error {
        Error::Deserialize(err, Backtrace::new())
    }

    pub fn serialize(err: bincode::Error) -> Error {
        Error::Serialize(err, Backtrace::new())
    }

    pub fn bind_socket(err: io::Error) -> Error {
        Error::BindSocket(err, Backtrace::new())
    }

    pub fn connect_socket(err: io::Error) -> Error {
        Error::ConnectSocket(err, Backtrace::new())
    }

    pub fn poll_register(err: io::Error) -> Error {
        Error::PollRegister(err, Backtrace::new())
    }

    pub fn poll_init(err: io::Error) -> Error {
        Error::PollInit(err, Backtrace::new())
    }
}
