// TODO: implement detecting and resending lost messages

use crate::networking::Error;
use byteorder::{ReadBytesExt, WriteBytesExt, BE};
use std::io::{Read, Write};

pub const HEADER_BYTES: usize = 8;

/// A wrapper over `UdpSocket` that implements optional reliable delivery.
pub struct Connection {
    local_sequence: u32,
    remote_sequence: u32,
    acks: u32,
}

impl Default for Connection {
    fn default() -> Connection {
        // Sequences start at one since 0 was the handshake.

        // TODO: if handshake is sent through the connection, switch
        // back to 0
        Connection {
            local_sequence: 1,
            remote_sequence: 1,
            acks: 0,
        }
    }
}

impl Connection {
    /// Processes the header of a received packet and returns it's
    /// sequence number.
    pub fn recv_header<B: Read>(&mut self, mut packet: B) -> Result<u32, Error> {
        let sequence = packet.read_u32::<BE>().map_err(Error::header_read)?;
        let acks = packet.read_u32::<BE>().map_err(Error::header_read)?;
        // TODO: handle packet loss

        if self.remote_sequence < sequence {
            self.acks <<= sequence - self.remote_sequence;
            self.acks |= 1;
            self.remote_sequence = sequence;
        } else if self.remote_sequence - sequence <= 32 {
            self.acks |= 1 << (self.remote_sequence - sequence);
        }
        Ok(sequence)
    }

    pub fn send_header<B: Write>(&mut self, mut packet: B) -> Result<(), Error> {
        packet
            .write_u32::<BE>(self.local_sequence)
            .map_err(Error::header_write)?;
        packet
            .write_u32::<BE>(self.acks)
            .map_err(Error::header_write)?;
        self.local_sequence += 1;

        Ok(())
    }
}
