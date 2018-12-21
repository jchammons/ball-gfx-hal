use crate::networking::connection::Acks;
use either::Either;
use log::warn;
use nalgebra::Point2;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::iter;
use std::time::Instant;

// At 60hz input, this is 1/2 second worth of inputs.
const MAX_INPUT_BUFFER: usize = 32;

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub struct Input {
    pub cursor: Point2<f32>,
}

/// Stores queued inputs in a ring buffer.
///
/// After receiving ACK for the sent input packets, stops keeping
/// track of those inputs.
#[derive(Debug)]
pub struct InputBuffer {
    /// Most recent input stored.
    latest: (Input, Instant),
    /// Total inputs that have been stored over the entire lifetime of
    /// the buffer. This is used to offset the input indices from
    /// `sent_packets`.xb
    input_count: usize,
    /// Keeps track of the input packets that have been sent but not
    /// acknowledged yet. Stores the sequence number for each packet,
    /// along with the absolute input index for the last input sent in
    /// that packet. In order to find the offset in the `inputs`
    /// array, `inputs_count` has to be taken into account.
    sent_packets: Vec<(u32, usize)>,
    /// Stores all buffered inputs that have not yet been acknowledged
    /// by the server, along with the delay between them.
    inputs: VecDeque<(Input, f32)>,
    /// Absolute index of the most recently sent packet.
    last_packet: usize,
}

impl InputBuffer {
    /// Creates a new input buffer with a given initial input.
    pub fn new(input: Input) -> InputBuffer {
        InputBuffer {
            latest: (input, Instant::now()),
            input_count: 0,
            sent_packets: Vec::new(),
            inputs: VecDeque::new(),
            last_packet: 0,
        }
    }

    /// Returns the delay since the last stored input or the last
    /// acknowledged packet, whichever is shorter.
    pub fn delay(&self, now: Instant) -> f32 {
        let last = self.latest.1;
        if now < last {
            0.0
        } else {
            now.duration_since(last).as_float_secs() as f32
        }
    }

    /// Returns all unacknowledged inputs, in order, along with the
    /// delay in seconds between each one.
    pub fn inputs(&self) -> impl Iterator<Item = (&Input, f32)> {
        self.inputs.iter().map(|(input, delay)| (input, *delay))
    }

    /// Returns the most recently stored input.
    pub fn latest(&self) -> &Input {
        &self.latest.0
    }

    /// Inserts a new input into the buffer.
    pub fn store_input(&mut self, input: Input, now: Instant) {
        self.input_count += 1;
        if self.inputs.len() == MAX_INPUT_BUFFER {
            warn!("input buffer overflowed");
            self.inputs.pop_front();
        }
        self.inputs.push_back((input, self.delay(now) as f32));
        self.latest = (input, now);
    }

    /// Returns all inputs that occured since the last packet.
    ///
    /// This also stores a new sent packet, using the provided
    /// sequence number.
    pub fn packet_send<'a>(
        &'a mut self,
        sequence: u32,
    ) -> impl Iterator<Item = Input> + 'a {
        if self.input_count != 0 {
            // Determine index offset of most recently sent packet in the
            // inputs array.
            let num_inputs = self.input_count - self.last_packet;
            // Determine the range of new inputs.
            let start = self.inputs.len() - num_inputs.min(MAX_INPUT_BUFFER);
            // Store the new packet as sent.
            self.sent_packets.push((sequence, self.input_count));
            self.last_packet = self.input_count;
            // Unfortunately, there isn't a nice way to get an iterator
            // for a range in VecDeque.
            Either::Left(
                (0..num_inputs)
                    .map(move |idx| self.inputs[idx + start].0.clone()),
            )
        } else {
            Either::Right(iter::empty())
        }
    }

    /// Clears out old buffered inputs corresponding to a set of
    /// acknowledged packets.
    ///
    /// Returns true if any new packets were acknowledged.
    pub fn packet_acks(&mut self, acks: Acks) -> bool {
        // Find the most recent acknowledged packet that was actually
        // an input packet.
        let sequence = self
            .sent_packets
            .iter()
            .map(|&(sequence, _)| sequence)
            .filter(|&sequence| acks.contains(sequence))
            .max();
        let sequence = match sequence {
            Some(sequence) => sequence,
            None => return false,
        };

        // Since packets may be acknowledged out of order, there's not
        // really a nice way to check which ones are older without
        // iterating through the entire thing.
        let mut clear_to = 0;
        self.sent_packets.retain(|&(sent_sequence, end)| {
            let old = sent_sequence <= sequence;
            if old {
                clear_to = clear_to.max(end);
            }
            !old
        });
        // Clear out old packets.
        let num_inputs = self.input_count - clear_to;
        let end = self.inputs.len() - num_inputs.min(MAX_INPUT_BUFFER);
        self.inputs.drain(..end);

        true
    }
}
