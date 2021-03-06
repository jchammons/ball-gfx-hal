use crate::networking::RecvError;
use byteorder::{ReadBytesExt, WriteBytesExt, BE};
use serde::de::DeserializeOwned;
use smallvec::SmallVec;
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
    pub remote_acks: Acks,
}

impl Acks {
    /// Acknowledges a new received packet.
    pub fn ack(&mut self, sequence: u32) {
        if sequence > self.ack {
            // Packet newer than most recent packet, so shift
            // everything.
            self.ack_bits = self.ack_bits.wrapping_shl(sequence - self.ack);
            self.ack_bits |= 1;
            self.ack = sequence;
        } else {
            // Received a packet newer than this one before.
            self.ack_bits |= 1u32.wrapping_shl(self.ack - sequence);
        }
    }

    /// Combines this with another set of acks, and returns the set of
    /// packets that can now be considered lost.
    pub fn combine(&mut self, new: Acks) -> SmallVec<[u32; 4]> {
        let mut lost = SmallVec::new();
        if new.ack > self.ack {
            // Anything that is outside the range of the new ack can
            // be considered lost.
            let mask = !((!0u32).wrapping_shr(new.ack - self.ack));
            lost.extend(
                Acks {
                    ack_bits: !self.ack_bits & mask,
                    ack: self.ack,
                }
                .iter(),
            );

            // Shift everything.
            self.ack_bits = self.ack_bits.wrapping_shl(new.ack - self.ack);
            self.ack_bits |= new.ack;
            self.ack = new.ack;
        } else {
            self.ack_bits |= new.ack.wrapping_shl(self.ack - new.ack);
        }
        lost
    }

    /// Checks if a particular sequence number is present in this set
    /// of acks.
    pub fn contains(self, sequence: u32) -> bool {
        if self.ack < sequence {
            return false;
        }
        self.ack_bits & (1u32.wrapping_shl(self.ack - sequence)) != 0
    }

    /// Returns an iterator over the acked packets.
    pub fn iter(self) -> impl Iterator<Item = u32> {
        (0..32.min(self.ack)).filter_map(move |offset| {
            if self.ack_bits & (1 << offset) != 0 {
                Some(self.ack - offset)
            } else {
                None
            }
        })
    }
}

impl Connection {
    /// Processes the header of a received packet and returns it's
    /// sequence number, as well as acknowledged packets and lost
    /// packets.
    pub fn recv_header<B: Read>(
        &mut self,
        mut packet: B,
    ) -> Result<(u32, Acks, SmallVec<[u32; 4]>), RecvError> {
        let sequence =
            packet.read_u32::<BE>().map_err(RecvError::header_read)?;
        let ack = packet.read_u32::<BE>().map_err(RecvError::header_read)?;
        let ack_bits =
            packet.read_u32::<BE>().map_err(RecvError::header_read)?;

        self.acks.ack(sequence);
        let acks = Acks {
            ack_bits,
            ack,
        };
        let lost = self.remote_acks.combine(acks);
        Ok((sequence, acks, lost))
    }

    /// Writes the header for the next packet into a buffer.
    ///
    /// Panics if the buffer is not large enough or if an IO error
    /// occurs while writing.
    pub fn send_header<B: Write>(&mut self, mut packet: B) -> u32 {
        let sequence = self.local_sequence;
        self.local_sequence += 1;
        packet.write_u32::<BE>(sequence).unwrap();
        packet.write_u32::<BE>(self.acks.ack).unwrap();
        packet.write_u32::<BE>(self.acks.ack_bits).unwrap();
        sequence
    }

    /// Reads the header of a packet, and then deserializes the
    /// contents with serde. Returns the sequence numbers of packets
    /// that are now considered lost.
    pub fn decode<B: Read, P: DeserializeOwned>(
        &mut self,
        mut read: B,
    ) -> Result<(P, u32, Acks, SmallVec<[u32; 4]>), RecvError> {
        let (sequence, acks, lost) = self.recv_header(&mut read)?;
        let packet =
            bincode::deserialize_from(read).map_err(RecvError::deserialize)?;
        Ok((packet, sequence, acks, lost))
    }
}
