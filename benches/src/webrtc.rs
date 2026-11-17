//! The workloads of the WebRTC benchmarks: one packet of a call, taken
//! in and sent.
//!
//! A call is set up in memory between the end that answers and a client
//! that offers what a browser offers, and is run to the middle of
//! itself. What is measured then is what one packet costs the server:
//! a datagram taken in, opened and handed on as the frames of audio it
//! holds, or a frame of a response sealed into the datagrams it leaves
//! in. With a client that offers redundant audio, as Chrome does, a
//! datagram carries the frame before beside its own, both ways.
//!
//! The counts include `str0m` and the ciphers.

use waveport_transport_webrtc::loopback::Pair;
use waveport_transport_webrtc::{Client, Like};

/// How many frames go each way before one is measured.
const BEFORE: u8 = 50;

/// A packet of 20 ms, as long as speech at 32 kbit/s makes one. What
/// follows its first byte is of no account to a transport.
fn packet(n: u8) -> Vec<u8> {
    let mut packet = vec![0xF8];
    packet.extend((0..79_u8).map(|at| at.wrapping_mul(31).wrapping_add(n)));
    packet
}

/// A call in the middle of itself.
fn call(like: Like) -> Pair {
    let Some(mut pair) = Pair::connected(like, Client::V1) else {
        // Two ends in one memory connect, or the crate is broken.
        std::process::abort();
    };
    for n in 0..BEFORE {
        for datagram in pair.said(&packet(n)) {
            pair.take(&datagram);
        }
        pair.send(&packet(n));
    }
    pair
}

/// A call whose far end has just sent a packet.
#[derive(Debug)]
pub struct Inbound {
    pair: Pair,
    datagram: Vec<u8>,
}

impl Inbound {
    /// A call with a client that offers as `like` does, in the middle
    /// of itself.
    #[must_use]
    pub fn prepare(like: Like) -> Box<Self> {
        let mut pair = call(like);
        let datagram = pair.said(&packet(BEFORE)).pop().unwrap_or_default();
        Box::new(Self { pair, datagram })
    }

    /// Takes the packet in. Returns how many frames of audio it gave:
    /// one, on a line that lost nothing.
    pub fn packet(&mut self) -> usize {
        self.pair.take(&self.datagram)
    }
}

/// A call whose session has a frame to send.
#[derive(Debug)]
pub struct Outbound {
    pair: Pair,
    frame: Vec<u8>,
}

impl Outbound {
    /// A call with a client that offers as `like` does, in the middle
    /// of itself.
    #[must_use]
    pub fn prepare(like: Like) -> Box<Self> {
        let mut pair = call(like);
        pair.tick();
        Box::new(Self {
            pair,
            frame: packet(BEFORE),
        })
    }

    /// Sends the frame. Returns how many datagrams it left in: one.
    pub fn frame(&mut self) -> usize {
        self.pair.send(&self.frame).len()
    }
}

#[cfg(test)]
mod tests {
    use waveport_transport_webrtc::Like;

    use super::{Inbound, Outbound};

    #[test]
    fn a_packet_is_one_frame_in_and_one_datagram_out() {
        for like in [Like::Chrome, Like::Firefox] {
            assert_eq!(Inbound::prepare(like).packet(), 1, "{like:?}");
            assert_eq!(Outbound::prepare(like).frame(), 1, "{like:?}");
        }
    }
}
