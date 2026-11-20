//! One call, as the end that answers has it.
//!
//! A [`Call`] does nothing by itself: it is given what arrived and what
//! time it is, and says what is to be sent, what happened and when it
//! wants to be asked again. Whoever holds it has the sockets and the
//! clock, which is what lets a test run a call on a clock of its own.

use core::time::Duration;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Instant;

use bytes::Bytes;
use str0m::change::SdpOffer;
use str0m::channel::{ChannelConfig, ChannelId};
use str0m::media::MediaKind;
use str0m::net::{Protocol, TcpType};
use str0m::{Candidate, Event, IceConnectionState};
use waveport_adapter_kit::client::{read, write};
use waveport_adapter_kit::line::Runs;
use waveport_domain::event::Key;
use waveport_domain::ids::OpTag;
use waveport_domain::time::MediaDur;
use waveport_protocol::client::v1::{ErrorCode, FromClient, FromServer};

use crate::crypto::Certificate;
use crate::dialect::{Dialect, Meant};
use crate::engine::{AGAIN, Core, Engine, Stepped, TURNS};
use crate::media::{CLOCK, In, Kinds, Media};
use crate::sent::Sent;
use crate::stack::{self, Offered};

/// The data channel a client that speaks the protocol and its server
/// agree on in advance, so that neither has to open it: its number and
/// its name.
pub const CHANNEL: u16 = 0;

/// What the channel is called.
pub const CHANNEL_LABEL: &str = "waveport";

/// How much may wait in the data channel for a client that is behind,
/// before whoever tells it more is made to wait.
const BEHIND: usize = 64 * 1024;

/// How many messages may wait for a data channel that is not open yet.
const EARLY: usize = 32;

/// How long nothing at all may arrive from a far end before its line is
/// taken to have dropped. A browser on a call sends audio fifty times a
/// second, and one that sends none still sends its checks and its
/// reports every few seconds. `str0m` says so itself after fifteen
/// seconds, which is a long time to talk to nobody.
const SILENT: Duration = Duration::from_secs(5);

/// The longest the output of a far end is taken to need to sound what
/// it was given, in milliseconds. One that says it takes longer is held
/// to this.
const SLOWEST_OUTPUT_MS: u32 = 1_000;

/// What the far end of a call is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Client {
    /// One that speaks client protocol v1 on a data channel agreed in
    /// advance ([`CHANNEL`]): it says hello there, is told of the
    /// session, says what it has played, and mutes what it holds when
    /// it is told to discard it.
    V1,
    /// Any other: audio and keys. It is told nothing, so what it has
    /// played is worked out from the pace, and what it holds cannot be
    /// discarded. A data channel it opens is not read.
    Plain,
    /// One that speaks another protocol, on a data channel it opens
    /// itself: what it says there is put into client protocol v1, and
    /// what it is told into its own, by the
    /// [`Dialect`](crate::Dialect) its call was answered with. It does
    /// not say what it has played, and nothing it holds can be
    /// discarded: both are as for [`Plain`](Self::Plain).
    Other,
}

/// Why an offer was not answered. It says nothing of what the offer
/// held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unanswered {
    /// It is no offer, or not one `str0m` can answer.
    Unreadable,
    /// It asks for no audio, and a call is one of audio.
    NoAudio,
    /// It asks for no data channel, and the client is one that speaks
    /// on one.
    NoChannel,
    /// The call is to be one of a client that speaks another protocol,
    /// and nothing says which.
    NoDialect,
    /// The call is over.
    Over,
}

impl core::fmt::Display for Unanswered {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Unreadable => "the offer is not one that can be answered",
            Self::NoAudio => "the offer asks for no audio",
            Self::NoChannel => {
                "the offer asks for no data channel, which the protocol is spoken on"
            }
            Self::NoDialect => "nothing says what the far end speaks on its data channel",
            Self::Over => "the call is over",
        })
    }
}

impl core::error::Error for Unanswered {}

/// A share of one, as a number of a hundred.
fn hundredths(share: f32) -> u8 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the share is held between nought and a hundred first"
    )]
    let hundredths = (share.clamp(0.0, 1.0) * 100.0).round() as u8;
    hundredths
}

/// What a call is made with.
#[derive(Clone, Debug)]
pub(crate) struct Setup<'a> {
    pub(crate) certificate: &'a Certificate,
    pub(crate) dtls13: bool,
    pub(crate) client: Client,
    /// Where the far end reaches this one over UDP.
    pub(crate) udp: Option<SocketAddr>,
    /// And over TCP.
    pub(crate) tcp: Option<SocketAddr>,
}

/// Something that happened on a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Told {
    /// The far end is there: the call connected, or its line is back.
    Up,
    /// The far end cannot be reached. It may come back.
    Down,
    /// A frame of the far end's audio.
    Audio {
        sequence: u16,
        timestamp: u32,
        payload: Bytes,
    },
    /// The far end pressed a key.
    Key(Key),
    /// The far end said something in the protocol that is not the
    /// call's to deal with.
    Said(FromClient),
    /// The far end has played this much of a response.
    Played { tag: OpTag, played: MediaDur },
    /// Of the audio it was sent lately, the far end says this many
    /// packets in a hundred did not arrive.
    Lost(u8),
    /// The call is over: the far end closed it, or it failed.
    Over,
}

/// What a call wants next.
pub(crate) type Step = Stepped<Told>;

/// One call.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is something that is so or is not, and none follows from another"
)]
pub(crate) struct Call {
    engine: Engine,
    client: Client,
    media: Option<Media>,
    /// What the far end speaks on its channel, if not the protocol.
    dialect: Option<Box<dyn Dialect>>,
    /// The channel the protocol is spoken on.
    channel: Option<ChannelId>,
    open: bool,
    /// What is to be said on the channel once it is open.
    early: VecDeque<String>,
    /// Whether the handshake was ever done.
    connected: bool,
    up: bool,
    /// When something last arrived from the far end.
    heard_at: Instant,
    /// When the call was last given something or asked what it wants.
    asked_at: Instant,
    /// The place in what was sent that came out of the far end's buffer
    /// last, and when it is heard, while that is still to come.
    to_be_heard: Option<(u64, Instant)>,
    /// Whether `str0m` has given the far end up, until its checks
    /// succeed again.
    fell: bool,
    told: VecDeque<Told>,
    sent: Sent,
    /// What the far end's last receiver report counted up to, and how
    /// many packets it counted as lost by then.
    reported: Option<(u64, u64)>,
    /// How much was lost between its last two receiver reports, and in
    /// the last second by what it says of every packet, in hundredths.
    lost_by_report: u8,
    lost_by_packet: u8,
    /// The response audio was sent of last, as the far end knows it,
    /// and where in the stream that audio ended.
    ended: Option<(u32, u32)>,
    /// The name the far end's checks are addressed to.
    name: String,
    /// Whether it was said that the call is over.
    said_over: bool,
}

impl core::fmt::Debug for Call {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Call")
            .field("client", &self.client)
            .field("up", &self.up)
            .field("over", &self.engine.is_over())
            .finish_non_exhaustive()
    }
}

impl Call {
    /// Answers `offer`. Returns the call and the answer.
    pub(crate) fn answer(
        setup: &Setup<'_>,
        offer: &str,
        now: Instant,
    ) -> Result<(Self, String), Unanswered> {
        if setup.client == Client::Other {
            return Err(Unanswered::NoDialect);
        }
        Self::answered(setup, offer, now, None)
    }

    /// Answers the `offer` of a far end that speaks `dialect` on a
    /// data channel it opens. Returns the call and the answer.
    pub(crate) fn answer_in(
        setup: &Setup<'_>,
        offer: &str,
        now: Instant,
        dialect: Box<dyn Dialect>,
    ) -> Result<(Self, String), Unanswered> {
        let setup = Setup {
            client: Client::Other,
            ..setup.clone()
        };
        Self::answered(&setup, offer, now, Some(dialect))
    }

    fn answered(
        setup: &Setup<'_>,
        offer: &str,
        now: Instant,
        dialect: Option<Box<dyn Dialect>>,
    ) -> Result<(Self, String), Unanswered> {
        let offered = Offered::read(offer);
        if !offered.audio {
            return Err(Unanswered::NoAudio);
        }
        if setup.client != Client::Plain && !offered.channel {
            return Err(Unanswered::NoChannel);
        }
        let mut engine = Engine::new(
            stack::rtc(setup.certificate, offered.answered(setup.dtls13), now),
            now,
        );
        let made = engine.with(|rtc| {
            if let Some(udp) = setup.udp
                && let Ok(candidate) = Candidate::host(udp, "udp")
            {
                rtc.add_local_candidate(candidate);
            }
            if let Some(tcp) = setup.tcp {
                let candidate = Candidate::builder().tcp().host(tcp);
                if let Ok(candidate) = candidate.tcptype(TcpType::Passive).build() {
                    rtc.add_local_candidate(candidate);
                }
            }
            let channel = (setup.client == Client::V1).then(|| {
                rtc.direct_api().create_data_channel(ChannelConfig {
                    label: CHANNEL_LABEL.to_owned(),
                    negotiated: Some(CHANNEL),
                    ..ChannelConfig::default()
                })
            });
            let offer = SdpOffer::from_sdp_string(offer).ok()?;
            let answer = rtc.sdp_api().accept_offer(offer).ok()?;
            let name = rtc.direct_api().local_ice_credentials().ufrag;
            Some((channel, answer.to_sdp_string(), name))
        });
        let Some(Some((channel, answer, name))) = made else {
            return Err(Unanswered::Unreadable);
        };
        let mut call = Self {
            engine,
            client: setup.client,
            media: None,
            dialect,
            channel,
            open: false,
            early: VecDeque::new(),
            connected: false,
            up: false,
            heard_at: now,
            asked_at: now,
            to_be_heard: None,
            fell: false,
            told: VecDeque::new(),
            sent: Sent::default(),
            reported: None,
            lost_by_report: 0,
            lost_by_packet: 0,
            ended: None,
            name,
            said_over: false,
        };
        call.drain();
        Ok((call, answer))
    }

    /// Answers another offer of the same call: the far end changed
    /// where it is, and begins its checks anew.
    pub(crate) fn answer_again(&mut self, offer: &str) -> Result<String, Unanswered> {
        let made = self.engine.with(|rtc| {
            let offer = SdpOffer::from_sdp_string(offer).ok()?;
            let answer = rtc.sdp_api().accept_offer(offer).ok()?;
            let name = rtc.direct_api().local_ice_credentials().ufrag;
            Some((answer.to_sdp_string(), name))
        });
        match made {
            Some(Some((answer, name))) => {
                self.name = name;
                self.drain();
                Ok(answer)
            }
            // The call goes on as it was.
            Some(None) => Err(Unanswered::Unreadable),
            None => Err(Unanswered::Over),
        }
    }

    /// The name the far end's checks are addressed to, which is how a
    /// datagram from an address nobody knows yet finds its call.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) const fn client(&self) -> Client {
        self.client
    }

    /// What was agreed for the audio, once the call has a line of it.
    pub(crate) fn kinds(&self) -> Option<Kinds> {
        self.media.as_ref().map(Media::kinds)
    }

    /// Whether the call is over.
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
        self.asked_at = now;
        if !self.engine.receive(now, protocol, from, to, datagram) {
            // Somebody else's: it does not make a far end that fell
            // silent one that was heard.
            return false;
        }
        self.heard_at = now;
        self.drain();
        // A far end that was silent is back.
        if self.connected && !self.fell {
            self.rises();
        }
        true
    }

    /// When the call wants to be asked again, if nothing arrives
    /// before.
    fn due(&self, now: Instant) -> Instant {
        let keys = self.media.as_ref().and_then(Media::keys_due);
        let wake = self.engine.wake(now);
        let wake = keys.map_or(wake, |keys| wake.min(keys));
        let wake = self.to_be_heard.map_or(wake, |(_, when)| wake.min(when));
        if self.up {
            wake.min(self.heard_at + SILENT)
        } else {
            wake
        }
    }

    /// Has the library say `times` more times that its time is now,
    /// and returns how often it was given the time so far.
    #[cfg(test)]
    pub(crate) fn stalls(&mut self, times: u32) -> u32 {
        self.engine.stalls = times;
        self.engine.ticks
    }

    /// What the call wants next, at `now`.
    pub(crate) fn poll(&mut self, now: Instant) -> Step {
        self.asked_at = now;
        let mut turns = 0;
        loop {
            if let Some(transmit) = self.engine.transmit() {
                return Step::Send(transmit);
            }
            if let Some(told) = self.told.pop_front() {
                return Step::Told(told);
            }
            if let Some((place, when)) = self.to_be_heard
                && now >= when
            {
                // What came out of the far end's buffer has been heard
                // by now, whether or not the far end says more.
                self.to_be_heard = None;
                self.hears(place);
                continue;
            }
            if self.engine.is_over() {
                if self.said_over {
                    return Step::Wait(self.engine.wake(now));
                }
                self.said_over = true;
                return Step::Told(Told::Over);
            }
            if self.up && now.saturating_duration_since(self.heard_at) >= SILENT {
                self.up = false;
                return Step::Told(Told::Down);
            }
            let due = self.due(now);
            if due > now {
                return Step::Wait(due);
            }
            // Its time has come.
            turns += 1;
            if turns > TURNS {
                return Step::Wait(now + AGAIN);
            }
            if let Some(media) = &mut self.media {
                self.engine.with(|rtc| media.press_on(rtc, now));
            }
            self.engine.tick(now);
            self.drain();
        }
    }

    /// Takes what `str0m` says happened.
    fn happened(&mut self, event: Event) {
        match event {
            Event::Connected => {
                self.connected = true;
                self.rises();
            }
            Event::IceConnectionStateChange(state) => match state {
                IceConnectionState::Disconnected => {
                    self.fell = true;
                    if self.up {
                        self.up = false;
                        self.told.push_back(Told::Down);
                    }
                }
                IceConnectionState::Connected | IceConnectionState::Completed => {
                    self.fell = false;
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
            Event::ChannelOpen(id, label) => {
                // A far end that speaks another protocol opens the
                // channel it is spoken on, and names it: the first of
                // that name is the one.
                let named = self.dialect.as_ref().map(|dialect| dialect.channel());
                if self.channel.is_none() && named == Some(label.as_str()) {
                    self.channel = Some(id);
                }
                if self.channel == Some(id) && !self.open {
                    self.open = true;
                    while let Some(early) = self.early.pop_front() {
                        self.write(&early);
                    }
                }
            }
            Event::ChannelData(data) => {
                // The protocol is spoken on the channel that was agreed
                // on, and on no other a far end may open.
                if self.channel == Some(data.id) {
                    self.said(data.binary, &data.data);
                }
            }
            Event::RtpPacket(packet) => {
                if let Some(media) = &mut self.media {
                    let told = &mut self.told;
                    media.heard(&packet, |heard| {
                        told.push_back(match heard {
                            In::Audio {
                                sequence,
                                timestamp,
                                payload,
                            } => Told::Audio {
                                sequence,
                                timestamp,
                                payload,
                            },
                            In::Key(key) => Told::Key(key),
                        });
                    });
                }
            }
            Event::PeerStats(stats) => {
                // What the far end says of every packet it was sent
                // (transport-wide feedback), a few times a second.
                if let Some(lost) = stats.egress_loss_fraction {
                    self.lost_by_packet = hundredths(lost);
                    self.says_lost();
                }
            }
            Event::MediaEgressStats(stats) => {
                // Its receiver reports, every few seconds: how many
                // packets it counts as lost in all, up to which one.
                let Some(report) = stats.remote else {
                    return;
                };
                let upto = *report.maximum_sequence_number;
                let before = self.media.as_ref().map(Media::first);
                let (from, lost_before) = match self.reported {
                    Some(reported) => reported,
                    None => (before.unwrap_or(upto).saturating_sub(1), 0),
                };
                if upto > from {
                    let lost = report.packets_lost.saturating_sub(lost_before);
                    #[expect(clippy::cast_precision_loss, reason = "a count of packets is small")]
                    let share = lost as f32 / (upto - from) as f32;
                    self.lost_by_report = hundredths(share);
                    self.reported = Some((upto, report.packets_lost));
                    self.says_lost();
                }
            }
            // A call that was closed says no more, which is how it is
            // known to be over.
            _ => {}
        }
    }

    /// Says how much of what it was sent the far end has lately not
    /// got: the more of what its two ways of saying so say. What it
    /// says of every packet is said every second and shows a loss as
    /// it begins; a receiver report counts every packet there was, and
    /// holds the figure up between two of them.
    fn says_lost(&mut self) {
        let lost = self.lost_by_report.max(self.lost_by_packet);
        self.told.push_back(Told::Lost(lost));
    }

    /// The far end is there.
    fn rises(&mut self) {
        if !self.up {
            self.up = true;
            self.told.push_back(Told::Up);
        }
    }

    /// Takes what the far end said on the data channel.
    fn said(&mut self, binary: bool, said: &[u8]) {
        let text = core::str::from_utf8(said).ok().filter(|_| !binary);
        let said = if let Some(dialect) = &mut self.dialect {
            // What is no text is nothing in a protocol that is written.
            let Some(text) = text else {
                return;
            };
            let Meant { means, answers } = dialect.heard(text);
            for answer in answers {
                self.says(answer);
            }
            let Some(said) = means else {
                return;
            };
            said
        } else {
            let Some(Ok(said)) = text.map(read::<FromClient>) else {
                self.refuse(
                    ErrorCode::Malformed,
                    "that is not a message of the protocol",
                );
                return;
            };
            said
        };
        if !matches!(said, FromClient::Hello { .. })
            && let Err(refusal) = said.check()
        {
            self.refuse(refusal.code(), &refusal.to_string());
            return;
        }
        match said {
            FromClient::PlayedRtp {
                ts,
                emitted,
                concealed,
                delay_ms,
            } => self.played(ts, emitted, concealed, delay_ms),
            FromClient::Played { .. } => {
                let why = "on a call what was played is said by its place in the stream";
                self.refuse(ErrorCode::Unsupported, why);
            }
            FromClient::Unknown => {}
            other => self.told.push_back(Told::Said(other)),
        }
    }

    /// The far end says where in the stream it is.
    fn played(
        &mut self,
        time: Option<u32>,
        emitted: Option<u64>,
        concealed: Option<u64>,
        delay_ms: u32,
    ) {
        // The time of the audio that was played last says which audio
        // it was. How much came out says only how much, and counts
        // nothing for audio that was lost: it is what a browser has
        // that cannot say the first, or stops short of it.
        let place = match (time, emitted) {
            (Some(time), _) => self.sent.before(time),
            (None, Some(emitted)) => Some(emitted.saturating_sub(concealed.unwrap_or(0))),
            (None, None) => None,
        };
        let Some(place) = place else {
            return;
        };
        // What came out of the buffer is heard this much later: so
        // much less is heard now, and all of it when that time has
        // passed. It is not left to the far end to say so then. One
        // that has played a response to its end has nothing new to
        // say, and its response would stay short of its end by what
        // its output holds.
        let delay_ms = delay_ms.min(SLOWEST_OUTPUT_MS);
        let unheard = u64::from(delay_ms) * u64::from(CLOCK / 1_000);
        self.hears(place.saturating_sub(unheard));
        let when = self.asked_at + Duration::from_millis(u64::from(delay_ms));
        self.to_be_heard = (unheard > 0).then_some((place, when));
    }

    /// The far end has heard what was sent as far as `place`.
    fn hears(&mut self, place: u64) {
        let told = &mut self.told;
        self.sent.played(place, |tag, played| {
            let played = MediaDur::from_ticks(played);
            told.push_back(Told::Played { tag, played });
        });
    }

    /// Tells the far end that something it said was not taken.
    fn refuse(&mut self, code: ErrorCode, why: &str) {
        let message = why.to_owned();
        self.tell(&FromServer::Error { code, message });
    }

    /// Writes `text` on the channel. Returns whether it was taken.
    fn write(&mut self, text: &str) -> bool {
        let Some(id) = self.channel else {
            return false;
        };
        // A channel that holds as much as it may takes no more, and
        // says so: that is not written.
        let written = self.engine.with(|rtc| {
            rtc.channel(id).is_some_and(|mut channel| {
                matches!(channel.write(false, text.as_bytes()), Ok(true))
            })
        });
        self.drain();
        written.unwrap_or(false)
    }

    /// Whether the far end has everything it was told: nothing of it
    /// waits to be sent, or to be said to have arrived. So has a far
    /// end that is told nothing, and one whose call is over.
    pub(crate) fn has_told_all(&mut self) -> bool {
        let Some(id) = self.channel.filter(|_| self.open) else {
            return self.early.is_empty();
        };
        let waiting = self.engine.with(|rtc| {
            rtc.channel(id)
                .map_or(0, |mut channel| channel.buffered_amount())
        });
        waiting.unwrap_or(0) == 0
    }

    /// Tells the far end `message`, if it is one that is told things:
    /// as it is, or as the far end's own protocol says it, which may
    /// be in several messages or in none. Returns whether it will be.
    pub(crate) fn tell(&mut self, message: &FromServer) -> bool {
        if self.client == Client::Plain || self.engine.is_over() {
            return false;
        }
        let Some(dialect) = &mut self.dialect else {
            return self.says(write(message));
        };
        let mut all = true;
        for text in dialect.told(message) {
            all &= self.says(text);
        }
        all
    }

    /// Says `text` on the channel, or keeps it for a channel that is
    /// not open yet. Returns whether it will be said.
    fn says(&mut self, text: String) -> bool {
        if self.open {
            return self.write(&text);
        }
        // The channel opens with the call, or a moment after it where
        // the far end opens it. What is said before waits.
        if self.early.len() >= EARLY {
            return false;
        }
        self.early.push_back(text);
        true
    }

    /// Whether the far end keeps up with what it is told: whoever tells
    /// it more waits while it does not.
    pub(crate) fn has_room(&mut self) -> bool {
        let Some(id) = self.channel.filter(|_| self.open) else {
            return self.early.len() < EARLY;
        };
        let room = self.engine.with(|rtc| {
            rtc.channel(id)
                .is_none_or(|mut channel| channel.buffered_amount() < BEHIND)
        });
        room.unwrap_or(false)
    }

    /// Sends a frame of the response `tag`, `samples` long, and tells a
    /// far end that is told things where the response begins and ends.
    pub(crate) fn frame(
        &mut self,
        now: Instant,
        runs: &mut Runs,
        tag: OpTag,
        samples: u32,
        audio: &[u8],
        last: bool,
    ) {
        if self.engine.is_over() {
            return;
        }
        let (number, generation) = {
            let run = runs.of(tag);
            run.sent = run.sent.saturating_add(samples);
            (run.number, tag.generation.get())
        };
        // A response that ends with a frame of nothing ends where its
        // audio did.
        let mut ends = self
            .ended
            .and_then(|(run, ends)| (run == number).then_some(ends));
        if !audio.is_empty() {
            let media = &mut self.media;
            let sent = self
                .engine
                .with(|rtc| media.as_mut()?.send(rtc, now, audio, samples));
            if let Some(placed) = sent.flatten() {
                if self.client == Client::V1 {
                    self.sent.add(tag, placed.time, samples);
                }
                if runs.begins(number) {
                    self.tell(&FromServer::Mark {
                        run: number,
                        generation,
                        end: false,
                        ts: Some(placed.time),
                    });
                }
                ends = Some(placed.time.wrapping_add(samples));
                self.ended = ends.map(|ends| (number, ends));
            }
            self.drain();
        }
        if last {
            runs.ends(number);
            self.tell(&FromServer::Mark {
                run: number,
                generation,
                end: true,
                ts: ends,
            });
        }
    }

    /// Tells the far end to discard what it holds of the response
    /// `tag`, if anything of it was sent.
    pub(crate) fn clear(&mut self, runs: &mut Runs, tag: OpTag) {
        let Some(run) = runs.tagged(tag) else {
            return;
        };
        runs.ends(run.number);
        self.tell(&FromServer::Clear { run: run.number });
    }

    /// Presses `key` at the far end. Returns whether it will be.
    pub(crate) fn press(&mut self, now: Instant, key: Key) -> bool {
        let media = self.media.as_mut().filter(|_| !self.engine.is_over());
        media.is_some_and(|media| media.press(now, key))
    }

    /// Sends a frame of audio that is of no response: it is told of
    /// to nobody, and counts for nothing that was played.
    pub(crate) fn tone(&mut self, now: Instant, samples: u32, audio: &[u8]) {
        if audio.is_empty() {
            return;
        }
        let media = &mut self.media;
        self.engine
            .with(|rtc| media.as_mut()?.send(rtc, now, audio, samples));
        self.drain();
    }

    /// Ends the call: the far end is told in the call's own ways, and
    /// what was said before still goes out. The call is over once that
    /// is done. One whose channel is still being set up is over at
    /// once, and its far end is told nothing
    /// ([`Engine::close`]).
    pub(crate) fn close(&mut self) {
        self.engine.close();
        self.drain();
    }
}

impl Core for Call {
    type Told = Told;

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

    fn step(&mut self, now: Instant) -> Step {
        self.poll(now)
    }
}
