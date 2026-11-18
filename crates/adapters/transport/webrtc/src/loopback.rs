//! A call in memory: the end that answers and a client without a
//! browser, with the datagrams between them carried by whoever holds
//! the pair, on a clock that moves when it is told to.
//!
//! Neither end of a call does anything by itself, so a call can be run
//! without a socket and without waiting. That is what the tests of this
//! crate do with a network that loses what a seed says it loses, and
//! what the benchmarks do to count what a packet costs.

use core::time::Duration;
use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Instant;

use str0m::net::Protocol;
use waveport_adapter_kit::line::Runs;
use waveport_domain::ids::{Epoch, GenId, OpTag};

use crate::call::{Call, Client, Setup, Step, Told};
use crate::crypto::certificate;
use crate::peer::{Like, Offers, Peer, PeerHeard, PeerStep};

/// Where the two ends are, in the ranges that are set aside for
/// examples (RFC 5737).
const SERVER: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 10), 40_000));
const CLIENT: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 7), 50_000));

/// How long a frame of audio lasts.
const FRAME: Duration = Duration::from_millis(20);

/// How long two ends in one memory are given to connect.
const CONNECTS_WITHIN: Duration = Duration::from_secs(10);

/// What the network between the two ends does to audio. Everything
/// else arrives: a call that cannot be set up says nothing about its
/// audio.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Weather {
    /// How long a datagram takes.
    pub(crate) delay: Duration,
    /// The share of the client's packets of audio that are lost, from
    /// 0 to 1.
    pub(crate) lost_to_server: f64,
    /// The share of the server's.
    pub(crate) lost_to_client: f64,
    /// What decides which ones.
    pub(crate) seed: u64,
    /// Whether nothing arrives at all, as on a line that has dropped.
    pub(crate) cut: bool,
}

/// xorshift64*: the same losses for the same seed.
#[derive(Clone, Copy, Debug)]
struct Chance(u64);

impl Chance {
    const fn of(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    /// A number from 0 to 1.
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let high = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
        #[expect(clippy::cast_precision_loss, reason = "53 bits are held exactly")]
        let share = high as f64 / (1_u64 << 53) as f64;
        share
    }
}

/// Whether a datagram is a packet of audio, and not a report about it
/// or part of the call's set-up.
fn is_audio(datagram: &[u8]) -> bool {
    matches!(datagram, [128..=191, kind, ..] if !(192..=223).contains(kind))
}

/// A datagram on its way.
#[derive(Debug)]
struct Flying {
    arrives: Instant,
    to_server: bool,
    datagram: Vec<u8>,
}

/// The two ends of a call, and what is between them.
#[derive(Debug)]
pub struct Pair {
    pub(crate) call: Call,
    pub(crate) peer: Peer,
    pub(crate) runs: Runs,
    /// Where the client is.
    pub(crate) client: SocketAddr,
    now: Instant,
    weather: Weather,
    chance: Chance,
    flying: VecDeque<Flying>,
    /// What happened at the server, the oldest first, but for what the
    /// client said was lost.
    pub(crate) told: Vec<Told>,
    /// What the client said was lost of what it was sent, in
    /// hundredths, each time it said so.
    pub(crate) lost: Vec<u8>,
    /// What happened at the client.
    pub(crate) heard: Vec<PeerHeard>,
    /// The datagrams the last thing made, where they are asked for
    /// instead of carried.
    made: Vec<Vec<u8>>,
    /// Whether datagrams are carried at all.
    carried: bool,
    /// How many datagrams that one end sent the other said were not
    /// its call's.
    #[cfg(test)]
    pub(crate) refused: usize,
    /// The offer the call began with, and its answer.
    #[cfg(test)]
    pub(crate) first: (String, String),
}

/// The response every frame of a benchmark is of.
const RESPONSE: OpTag = OpTag {
    generation: GenId(1),
    epoch: Epoch(1),
};

impl Pair {
    /// Two ends that are connected: a client that offers as `like`
    /// does, and an end that answers it as it answers `client`. `None`
    /// if they do not connect, which is a fault of this crate.
    #[must_use]
    pub fn connected(like: Like, client: Client) -> Option<Self> {
        Self::begun(like, client, Weather::default())?.connects()
    }

    /// Lets the two ends connect. `None` if they do not.
    pub(crate) fn connects(self) -> Option<Self> {
        let mut pair = self;
        let by = pair.now + CONNECTS_WITHIN;
        while !pair.is_up() {
            if pair.now >= by {
                return None;
            }
            pair.run_for(Duration::from_millis(5));
        }
        Some(pair)
    }

    /// Two ends that have an offer and its answer, and have sent each
    /// other nothing yet.
    pub(crate) fn begun(like: Like, client: Client, weather: Weather) -> Option<Self> {
        let offers = Offers {
            like,
            speaks: client == Client::V1,
            tcp: false,
            dtls13: false,
        };
        Self::begun_as(offers, client, weather, false)
    }

    /// As [`begun`](Self::begun), with all that the client offers, and
    /// whether the server may make its handshake in DTLS 1.3.
    pub(crate) fn begun_as(
        offers: Offers,
        client: Client,
        weather: Weather,
        server13: bool,
    ) -> Option<Self> {
        let now = Instant::now();
        let (mut peer, offer) = Peer::offer(&certificate()?, offers, CLIENT, now)?;
        let certificate = certificate()?;
        let setup = Setup {
            certificate: &certificate,
            dtls13: server13,
            client,
            udp: Some(SERVER),
            tcp: None,
        };
        let (call, answer) = Call::answer(&setup, &offer, now).ok()?;
        if !peer.answered(&answer) {
            return None;
        }
        Some(Self {
            call,
            peer,
            runs: Runs::default(),
            client: CLIENT,
            now,
            weather,
            chance: Chance::of(weather.seed),
            flying: VecDeque::new(),
            told: Vec::new(),
            lost: Vec::new(),
            heard: Vec::new(),
            made: Vec::new(),
            carried: true,
            #[cfg(test)]
            refused: 0,
            #[cfg(test)]
            first: (offer, answer),
        })
    }

    /// Whether both ends have said that the call is up, and know what
    /// its audio is.
    fn is_up(&self) -> bool {
        let both = self.told.contains(&Told::Up) && self.heard.contains(&PeerHeard::Up);
        both && self.call.kinds().is_some() && self.peer.kinds().is_some()
    }

    /// What time it is for the two ends.
    #[cfg(test)]
    pub(crate) const fn now(&self) -> Instant {
        self.now
    }

    #[cfg(test)]
    pub(crate) const fn set_weather(&mut self, weather: Weather) {
        self.weather = weather;
        self.chance = Chance::of(weather.seed);
    }

    /// Lets `time` go by with nothing carried, and returns the datagrams
    /// the two ends made in it.
    #[cfg(test)]
    pub(crate) fn made_in(&mut self, time: Duration) -> Vec<Vec<u8>> {
        self.carried = false;
        self.run_for(time);
        self.carried = true;
        core::mem::take(&mut self.made)
    }

    /// Somebody at `from` sends the server `datagram`. Returns whether
    /// the call took it for its own.
    #[cfg(test)]
    pub(crate) fn intrudes(&mut self, from: SocketAddr, datagram: &[u8]) -> bool {
        let now = self.now;
        let taken = self
            .call
            .receive(now, Protocol::Udp, from, SERVER, datagram);
        self.settle();
        taken
    }

    /// Puts a datagram on its way, unless the weather takes it.
    fn fly(&mut self, to_server: bool, datagram: &[u8]) {
        if !self.carried {
            self.made.push(datagram.to_vec());
            return;
        }
        let lost = if to_server {
            self.weather.lost_to_server
        } else {
            self.weather.lost_to_client
        };
        if self.weather.cut || lost > 0.0 && is_audio(datagram) && self.chance.next() < lost {
            return;
        }
        self.flying.push_back(Flying {
            arrives: self.now + self.weather.delay,
            to_server,
            datagram: datagram.to_vec(),
        });
    }

    /// Has both ends do what they have to do now. Returns when one of
    /// them, or a datagram, wants the clock to have moved on to.
    fn settle(&mut self) -> Instant {
        loop {
            let now = self.now;
            let mut arrived = false;
            while self.flying.front().is_some_and(|next| next.arrives <= now) {
                let Some(next) = self.flying.pop_front() else {
                    break;
                };
                arrived = true;
                let taken = if next.to_server {
                    let (from, to) = (self.client, SERVER);
                    self.call
                        .receive(now, Protocol::Udp, from, to, &next.datagram)
                } else {
                    let (from, to) = (SERVER, self.client);
                    self.peer
                        .receive(now, Protocol::Udp, from, to, &next.datagram)
                };
                #[cfg(test)]
                {
                    self.refused += usize::from(!taken);
                }
                let _ = taken;
            }
            let server = loop {
                match self.call.poll(now) {
                    Step::Send(transmit) => self.fly(false, &transmit.contents),
                    Step::Told(Told::Lost(lost)) => self.lost.push(lost),
                    Step::Told(told) => self.told.push(told),
                    Step::Wait(until) => break until,
                }
            };
            let client = loop {
                match self.peer.poll(now) {
                    PeerStep::Send(transmit) => self.fly(true, &transmit.contents),
                    PeerStep::Told(heard) => self.heard.push(heard),
                    PeerStep::Wait(until) => break until,
                }
            };
            let flight = self.flying.front().map(|next| next.arrives);
            let next = flight.map_or(server.min(client), |flight| flight.min(server).min(client));
            // What is due now, as a datagram with no way to go is, is
            // done before the clock moves.
            if next > now || !(arrived || flight.is_some_and(|flight| flight <= now)) {
                return next;
            }
        }
    }

    /// Lets `time` go by, with both ends doing what they do in it.
    pub fn run_for(&mut self, time: Duration) {
        let until = self.now + time;
        loop {
            let next = self.settle();
            if next > until {
                self.now = until;
                // So that what was due at the last moment is done.
                self.settle();
                return;
            }
            self.now = self.now.max(next);
        }
    }

    /// The client says a frame, one frame's time after the last.
    /// Returns the datagrams that makes, which are not carried: they
    /// are whoever asked's to hand to [`take`](Self::take).
    pub fn said(&mut self, opus: &[u8]) -> Vec<Vec<u8>> {
        self.run_for(FRAME);
        self.carried = false;
        self.peer.speak(self.now, opus);
        self.settle();
        self.carried = true;
        core::mem::take(&mut self.made)
    }

    /// The server takes `datagram` in and does all it does about it.
    /// Returns how many frames of audio it handed on.
    pub fn take(&mut self, datagram: &[u8]) -> usize {
        let before = self.told.len();
        let now = self.now;
        self.call
            .receive(now, Protocol::Udp, self.client, SERVER, datagram);
        self.settle();
        let new = self.told.get(before..).unwrap_or_default();
        new.iter()
            .filter(|told| matches!(told, Told::Audio { .. }))
            .count()
    }

    /// A frame's time goes by, with both ends doing what they do in
    /// it.
    pub fn tick(&mut self) {
        self.run_for(FRAME);
    }

    /// The server sends a frame of a response, now. Returns the
    /// datagrams that makes, which are not carried.
    pub fn send(&mut self, opus: &[u8]) -> Vec<Vec<u8>> {
        self.carried = false;
        let now = self.now;
        self.call
            .frame(now, &mut self.runs, RESPONSE, 960, opus, false);
        self.settle();
        self.carried = true;
        core::mem::take(&mut self.made)
    }
}
