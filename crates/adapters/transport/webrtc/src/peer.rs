//! One call, as the end that offers has it: a client without a
//! browser.
//!
//! Like [`Call`](crate::call::Call) it does nothing by itself. It
//! offers what a browser offers, so that the end that answers is held
//! to what it does with a browser: redundant audio or none, the keys of
//! a telephone on one clock or two, a data channel or none.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Instant;

use bytes::Bytes;
use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::channel::{ChannelConfig, ChannelId};
use str0m::media::{Direction, MediaKind, Mid};
use str0m::net::{Protocol, TcpType};
use str0m::{Candidate, Event, IceConnectionState};
use waveport_adapter_kit::client::{read, write};
use waveport_adapter_kit::line::opus_samples;
use waveport_domain::event::Key;
use waveport_protocol::client::v1::{FromClient, FromServer};

use crate::call::{CHANNEL, CHANNEL_LABEL};
use crate::crypto::Certificate;
use crate::engine::{AGAIN, Core, Engine, Stepped, TURNS};
use crate::media::{In, Kinds, Media};
use crate::stack::{self, KeyClocks, Wanted};

/// What a client offers: as the browsers do that were measured, or the
/// least a far end can.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Like {
    /// Redundant audio, and the keys of a telephone on the clock of the
    /// audio and on that of a telephone line.
    Chrome,
    /// No redundant audio, and the keys on the clock of a telephone
    /// line alone.
    Firefox,
    /// Opus and nothing else: no redundant audio, and no keys. It is
    /// what a library that is no browser may offer.
    Bare,
}

/// What a client offers, and how.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Offers {
    pub(crate) like: Like,
    /// Whether it offers the data channel of the protocol.
    pub(crate) speaks: bool,
    /// The data channel it opens itself, to speak another protocol on,
    /// if it does: one that offers the channel of the protocol does
    /// not.
    pub(crate) opens: Option<&'static str>,
    /// Whether it reaches its server over a connection it makes, and
    /// not with datagrams.
    pub(crate) tcp: bool,
    /// Whether it may make its handshake in DTLS 1.3.
    pub(crate) dtls13: bool,
}

/// Something that happened at the offering end of a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PeerHeard {
    /// The call connected, or its line is back.
    Up,
    /// The other end cannot be reached.
    Down,
    /// The data channel is open.
    Open,
    /// A frame of the other end's audio, and the RTP time it begins
    /// at.
    Audio { time: u32, payload: Bytes },
    /// The other end pressed a key.
    Key(Key),
    /// The other end said something in the protocol.
    Said(FromServer),
    /// The other end said something on the channel this end opened,
    /// in whatever is spoken there.
    Text(String),
    /// The call is over.
    Over,
}

/// What the offering end wants next.
pub(crate) type PeerStep = Stepped<PeerHeard>;

/// The offering end of one call.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is something that is so or is not, and none follows from another"
)]
pub(crate) struct Peer {
    engine: Engine,
    /// The line of audio it offered.
    mid: Mid,
    pending: Option<SdpPendingOffer>,
    /// Where it offers from next, while that offer is not answered.
    moves_to: Option<SocketAddr>,
    media: Option<Media>,
    channel: Option<ChannelId>,
    /// Whether the channel is one it opened to speak another protocol
    /// on.
    strange: bool,
    open: bool,
    connected: bool,
    up: bool,
    heard: VecDeque<PeerHeard>,
    /// Whether it was said that the call is over.
    said_over: bool,
}

impl core::fmt::Debug for Peer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Peer")
            .field("up", &self.up)
            .field("over", &self.engine.is_over())
            .finish_non_exhaustive()
    }
}

impl Peer {
    /// Makes an offer as `like` does, from `local`, over UDP or over
    /// TCP, with the data channel of the protocol if `speaks`. Returns
    /// the end and its offer.
    pub(crate) fn offer(
        certificate: &Certificate,
        offers: Offers,
        local: SocketAddr,
        now: Instant,
    ) -> Option<(Self, String)> {
        let Offers {
            like,
            speaks,
            opens,
            tcp,
            dtls13,
        } = offers;
        let wanted = Wanted {
            red: like == Like::Chrome,
            nack: false,
            keys: KeyClocks {
                at_48: like == Like::Chrome,
                at_8: like != Like::Bare,
            },
            lite: false,
            dtls13,
        };
        let mut engine = Engine::new(stack::rtc(certificate, wanted, now), now);
        let made = engine.with(|rtc| {
            let candidate = if tcp {
                let candidate = Candidate::builder().tcp().host(local);
                candidate.tcptype(TcpType::Active).build().ok()?
            } else {
                Candidate::host(local, "udp").ok()?
            };
            rtc.add_local_candidate(candidate);
            let mut changes = rtc.sdp_api();
            let mid = changes.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
            let channel = if speaks {
                Some(changes.add_channel_with_config(ChannelConfig {
                    label: CHANNEL_LABEL.to_owned(),
                    negotiated: Some(CHANNEL),
                    ..ChannelConfig::default()
                }))
            } else {
                // One it opens itself, as a browser does that was not
                // told of a channel agreed on in advance.
                opens.map(|label| changes.add_channel(label.to_owned()))
            };
            let (offer, pending) = changes.apply()?;
            Some((mid, channel, offer.to_sdp_string(), pending))
        });
        let (mid, channel, offer, pending) = made??;
        let mut peer = Self {
            engine,
            mid,
            pending: Some(pending),
            moves_to: None,
            media: None,
            channel,
            strange: !speaks && opens.is_some(),
            open: false,
            connected: false,
            up: false,
            heard: VecDeque::new(),
            said_over: false,
        };
        peer.drain();
        Some((peer, offer))
    }

    /// Offers again, from `local`: the client is somewhere else than it
    /// was, and its checks begin anew. Returns the offer.
    pub(crate) fn offer_again(&mut self, local: SocketAddr) -> Option<String> {
        let made = self.engine.with(|rtc| {
            let mut changes = rtc.sdp_api();
            // What it was reached at before is not offered again.
            changes.ice_restart(false);
            let (offer, pending) = changes.apply()?;
            Some((offer.to_sdp_string(), pending))
        });
        let (offer, pending) = made??;
        self.pending = Some(pending);
        self.moves_to = Some(local);
        self.drain();
        Some(offer)
    }

    /// Takes the answer to the offer. Returns whether it is one.
    pub(crate) fn answered(&mut self, answer: &str) -> bool {
        let Some(pending) = self.pending.take() else {
            return false;
        };
        let taken = self.engine.with(|rtc| {
            let answer = SdpAnswer::from_sdp_string(answer).ok()?;
            rtc.sdp_api().accept_answer(pending, answer).ok()
        });
        if taken.flatten().is_none() {
            return false;
        }
        // Where it was reached before is forgotten as the answer is
        // taken, so where it is now is said after.
        if let Some(local) = self.moves_to.take() {
            self.engine.with(|rtc| {
                if let Ok(candidate) = Candidate::host(local, "udp") {
                    rtc.add_local_candidate(candidate);
                }
            });
        }
        // The end that offers is not told of the line it offered: what
        // was agreed for it is there to be read.
        if self.media.is_none() {
            let mid = self.mid;
            self.media = self.engine.with(|rtc| Media::of(rtc, mid)).flatten();
        }
        self.drain();
        self.media.is_some()
    }

    /// What was agreed for the audio, once the call has a line of it.
    pub(crate) fn kinds(&self) -> Option<Kinds> {
        self.media.as_ref().map(Media::kinds)
    }

    #[cfg(test)]
    pub(crate) const fn is_over(&self) -> bool {
        self.engine.is_over()
    }

    /// Takes everything the call has to say.
    fn drain(&mut self) {
        while let Some(event) = self.engine.event() {
            self.happened(event);
        }
    }

    /// Something arrived for the call, at `to`, from `from`. Returns
    /// whether it is the call's.
    pub(crate) fn receive(
        &mut self,
        now: Instant,
        protocol: Protocol,
        from: SocketAddr,
        to: SocketAddr,
        datagram: &[u8],
    ) -> bool {
        let taken = self.engine.receive(now, protocol, from, to, datagram);
        self.drain();
        taken
    }

    /// When the end wants to be asked again, if nothing arrives before.
    fn due(&self, now: Instant) -> Instant {
        let keys = self.media.as_ref().and_then(Media::keys_due);
        let wake = self.engine.wake(now);
        keys.map_or(wake, |keys| wake.min(keys))
    }

    /// Has the library say `times` more times that its time is now,
    /// and returns how often it was given the time so far.
    #[cfg(test)]
    pub(crate) fn stalls(&mut self, times: u32) -> u32 {
        self.engine.stalls = times;
        self.engine.ticks
    }

    /// What the end wants next, at `now`.
    pub(crate) fn poll(&mut self, now: Instant) -> PeerStep {
        let mut turns = 0;
        loop {
            if let Some(transmit) = self.engine.transmit() {
                return PeerStep::Send(transmit);
            }
            if let Some(heard) = self.heard.pop_front() {
                return PeerStep::Told(heard);
            }
            if self.engine.is_over() {
                if self.said_over {
                    return PeerStep::Wait(self.engine.wake(now));
                }
                self.said_over = true;
                return PeerStep::Told(PeerHeard::Over);
            }
            let due = self.due(now);
            if due > now {
                return PeerStep::Wait(due);
            }
            turns += 1;
            if turns > TURNS {
                return PeerStep::Wait(now + AGAIN);
            }
            if let Some(media) = &mut self.media {
                self.engine.with(|rtc| media.press_on(rtc, now));
            }
            self.engine.tick(now);
            self.drain();
        }
    }

    fn happened(&mut self, event: Event) {
        match event {
            Event::Connected => {
                self.connected = true;
                self.rises();
            }
            Event::IceConnectionStateChange(state) => match state {
                IceConnectionState::Disconnected => {
                    if self.up {
                        self.up = false;
                        self.heard.push_back(PeerHeard::Down);
                    }
                }
                IceConnectionState::Connected | IceConnectionState::Completed => {
                    if self.connected {
                        self.rises();
                    }
                }
                IceConnectionState::New | IceConnectionState::Checking => {}
            },
            Event::MediaAdded(added) => {
                if added.kind == MediaKind::Audio && self.media.is_none() {
                    self.media = self.engine.with(|rtc| Media::of(rtc, added.mid)).flatten();
                }
            }
            Event::ChannelOpen(id, _) => {
                if self.channel == Some(id) && !self.open {
                    self.open = true;
                    self.heard.push_back(PeerHeard::Open);
                }
            }
            Event::ChannelData(data) => {
                let text = core::str::from_utf8(&data.data).ok();
                if self.strange {
                    if let Some(text) = text.filter(|_| !data.binary) {
                        self.heard.push_back(PeerHeard::Text(text.to_owned()));
                    }
                } else if let Some(Ok(said)) = text.map(read::<FromServer>) {
                    self.heard.push_back(PeerHeard::Said(said));
                }
            }
            Event::RtpPacket(packet) => {
                if let Some(media) = &mut self.media {
                    let all = &mut self.heard;
                    media.heard(&packet, |heard| {
                        all.push_back(match heard {
                            In::Audio {
                                timestamp, payload, ..
                            } => PeerHeard::Audio {
                                time: timestamp,
                                payload,
                            },
                            In::Key(key) => PeerHeard::Key(key),
                        });
                    });
                }
            }
            _ => {}
        }
    }

    fn rises(&mut self) {
        if !self.up {
            self.up = true;
            self.heard.push_back(PeerHeard::Up);
        }
    }

    /// Says one Opus packet. Returns whether there is a line to say it
    /// on.
    pub(crate) fn speak(&mut self, now: Instant, opus: &[u8]) -> bool {
        let Some(samples) = opus_samples(opus) else {
            return false;
        };
        let media = &mut self.media;
        let sent = self
            .engine
            .with(|rtc| media.as_mut()?.send(rtc, now, opus, samples));
        self.drain();
        sent.flatten().is_some()
    }

    /// Presses `key`. Returns whether it will be.
    pub(crate) fn press(&mut self, now: Instant, key: Key) -> bool {
        let media = self.media.as_mut().filter(|_| !self.engine.is_over());
        media.is_some_and(|media| media.press(now, key))
    }

    /// Whether every key that was pressed has been said.
    pub(crate) fn has_pressed(&self) -> bool {
        self.media
            .as_ref()
            .is_none_or(|media| media.keys_due().is_none())
    }

    /// Says `message` on the data channel. Returns whether there is one
    /// that is open.
    pub(crate) fn say(&mut self, message: &FromClient) -> bool {
        self.say_raw(false, write(message).as_bytes())
    }

    /// Opens another data channel than the one it has, called `label`,
    /// as a far end may at any time.
    #[cfg(test)]
    pub(crate) fn opens_another(&mut self, label: &str) {
        let config = ChannelConfig {
            label: label.to_owned(),
            ..ChannelConfig::default()
        };
        self.engine.with(|rtc| {
            rtc.direct_api().create_data_channel(config);
        });
        self.drain();
    }

    /// Says `said` on the data channel, as text or as what is none.
    /// Returns whether there is a channel that is open.
    pub(crate) fn say_raw(&mut self, binary: bool, said: &[u8]) -> bool {
        let Some(id) = self.channel.filter(|_| self.open) else {
            return false;
        };
        let written = self.engine.with(|rtc| {
            rtc.channel(id)
                .is_some_and(|mut channel| matches!(channel.write(binary, said), Ok(true)))
        });
        self.drain();
        written.unwrap_or(false)
    }

    /// Whether the far end has everything that was said: nothing of it
    /// waits to be sent, or to be said to have arrived. So has one
    /// whose call is over.
    pub(crate) fn has_said_all(&mut self) -> bool {
        let Some(id) = self.channel.filter(|_| self.open) else {
            return true;
        };
        let waiting = self.engine.with(|rtc| {
            rtc.channel(id)
                .map_or(0, |mut channel| channel.buffered_amount())
        });
        waiting.unwrap_or(0) == 0
    }

    /// Ends the call.
    pub(crate) fn close(&mut self) {
        self.engine.close();
        self.drain();
    }
}

impl Core for Peer {
    type Told = PeerHeard;

    fn receive(
        &mut self,
        now: Instant,
        protocol: Protocol,
        from: SocketAddr,
        to: SocketAddr,
        datagram: &[u8],
    ) -> bool {
        Self::receive(self, now, protocol, from, to, datagram)
    }

    fn step(&mut self, now: Instant) -> PeerStep {
        self.poll(now)
    }
}
