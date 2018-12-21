use failure::{Backtrace, Fail};
use std::io;

pub mod client;
pub mod connection;
pub mod event_loop;
pub mod server;
pub mod tick;

/// MTU will probably never be bigger than this, so if a received
/// packet is bigger, there are probably other problems.
pub const MAX_PACKET_SIZE: usize = 4096;

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
