// TODO: implement detecting and resending lost messages

use crate::networking::Error;
use byteorder::{ReadBytesExt, WriteBytesExt, BE};
use serde::de::DeserializeOwned;
use std::io::{Read, Write};

// 4 u32s
pub const HEADER_BYTES: usize = 4 + 4 + 4;

#[derive(Copy, Clone, Debug, Default)]
pub struct Acks {
    /// Bitfield of previous packets.
    ack_bits: u32,
    /// Sequence number of most recently received packet.
    ack: u32,
}

/// A wrapper over `UdpSocket` that implements optional reliable delivery.
#[derive(Clone, Debug, Default)]
pub struct Connection {
    pub local_sequence: u32,
    pub acks: Acks,
}

impl Acks {
    /// Acknowledges a new received packet.
    pub fn ack(&mut self, sequence: u32) {
        if sequence > self.ack {
            // Packet newer than most recent packet, so shift
            // everything.
            self.ack_bits <<= sequence - self.ack;
            self.ack_bits |= 1;
            self.ack = sequence;
        } else if self.ack - sequence <= 32 {
            // Received a packet newer than this one before, but it's
            // still in the 32-packet window, so store it.
            self.ack_bits |= 1 << (self.ack - sequence);
        }
    }

    /// Checks if a particular sequence number is present in this set
    /// of acks.
    pub fn contains(self, sequence: u32) -> bool {
        if self.ack < sequence {
            return false;
        }
        self.ack_bits | (1 << (self.ack - sequence)) != 0
    }

    /// Returns an iterator over the acked packets.
    pub fn iter(self) -> impl Iterator<Item = u32> {
        (0..32).filter_map(move |offset| {
            if self.ack_bits | (1 << offset) != 0 {
                Some(self.ack - offset)
            } else {
                None
            }
        })
    }
}

impl Connection {
    /// Processes the header of a received packet and returns it's
    /// sequence number, as well as acknowledged packets.
    pub fn recv_header<B: Read>(
        &mut self,
        mut packet: B,
    ) -> Result<(u32, Acks), Error> {
        let sequence = packet.read_u32::<BE>().map_err(Error::header_read)?;
        let ack = packet.read_u32::<BE>().map_err(Error::header_read)?;
        let ack_bits = packet.read_u32::<BE>().map_err(Error::header_read)?;

        self.acks.ack(sequence);
        Ok((
            sequence,
            Acks {
                ack_bits,
                ack,
            },
        ))
    }

    pub fn send_header<B: Write>(
        &mut self,
        mut packet: B,
    ) -> Result<(), Error> {
        packet
            .write_u32::<BE>(self.local_sequence)
            .map_err(Error::header_write)?;
        packet.write_u32::<BE>(self.acks.ack).map_err(Error::header_write)?;
        packet
            .write_u32::<BE>(self.acks.ack_bits)
            .map_err(Error::header_write)?;
        self.local_sequence += 1;

        Ok(())
    }

    /// Reads the header of a packet, and then deserializes the
    /// contents with serde.
    pub fn decode<B: Read, P: DeserializeOwned>(
        &mut self,
        mut read: B,
    ) -> Result<(P, u32, Acks), Error> {
        let (sequence, acks) = self.recv_header(&mut read)?;
        let packet =
            bincode::deserialize_from(&mut read).map_err(Error::deserialize)?;
        Ok((packet, sequence, acks))
    }
}
