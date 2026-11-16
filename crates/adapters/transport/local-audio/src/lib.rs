//! A session on this machine's microphone and loudspeaker.
//!
//! Stability: experimental
//!
//! The transport of a developer who talks to an agent at their desk, and
//! of a test that plays one audio file to an agent and keeps what it
//! answers. There is one participant, the person at the machine, and
//! audio is plain samples at 48 kHz both ways, in frames of 20 ms.
//!
//! What a session has to know about a far end is known here exactly,
//! because the far end is this machine:
//!
//! - How much of a response has been played is counted where the samples
//!   are handed to the sound system, and said every 40 ms and at the end.
//!   The listener hears them later by however long the sound system and
//!   the loudspeaker take, which is not known here: a few milliseconds to
//!   a few tens.
//! - A response that is to be discarded falls silent within 5 ms of the
//!   device next asking for audio, and nothing more of it is played.
//!
//! # Echo
//!
//! Nothing here takes the agent's voice out of what the microphone hears.
//! With a loudspeaker and a microphone in one room the agent hears itself,
//! takes that for the person speaking, and stops. Headphones are the
//! remedy. Where there are none, [`Options::half_duplex`] makes the
//! microphone deaf while the agent speaks and for a moment after: the
//! agent can then not be spoken over, and words said while it speaks are
//! lost.
//!
//! # Pace
//!
//! A sound card runs on its own clock, and asks for audio in pieces of
//! its own size. So a response is not begun the moment its first frame
//! arrives: it waits until what the card asks for at once is there, and
//! 25 ms more. With a card that works in pieces of 10 ms that is no wait
//! at all, since a session sends its first two frames together.

mod card;
mod line;
mod script;

use core::task::{Context, Poll};
use core::time::Duration;
use std::collections::VecDeque;

use bytes::Bytes;
use tokio::time::Instant;
use waveport_domain::ids::{OpTag, ParticipantId};
use waveport_domain::time::MediaDur;
use waveport_ports::audio::{Codec, Rate};
use waveport_ports::error::Full;
use waveport_ports::transport::{
    FrameOut, RemoteBuffer, Transport, TransportCaps, TransportControl, TransportEvent,
};

use crate::card::Card;
pub use crate::card::{Choice, DeviceError, DeviceInfo, devices};
pub use crate::line::Health;
use crate::line::{Cut, Line, Progress};
use crate::script::Person;
pub use crate::script::{Recording, Step, UnknownRate};

/// The rate of the audio, both ways.
pub const RATE: Rate = Rate::Hz48000;

/// How many samples a frame of the microphone's audio holds: 20 ms.
const FRAME: usize = 960;

/// [`FRAME`], on the clock of the stream.
const FRAME_TICKS: u32 = 960;

/// How much more of a response has to have been played before the
/// session is told again: 40 ms.
const REPORT_EVERY: u32 = 1_920;

/// How long the microphone stays deaf after the agent has stopped
/// speaking, when it is deaf while the agent speaks: the sound system
/// and the room still hold some of what was played.
const DEAF_AFTER: Duration = Duration::from_millis(300);

/// How many responses are remembered at most while the session does not
/// ask what became of them.
const MOST_RUNS: usize = 64;

/// How a transport is set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Who the person at the machine is, to the session.
    pub who: ParticipantId,
    /// Whether the microphone is deaf while the agent speaks. See the
    /// [crate's notes on echo](crate#echo).
    pub half_duplex: bool,
}

impl Default for Options {
    /// Participant 0, with a microphone that always hears.
    fn default() -> Self {
        Self {
            who: ParticipantId(0),
            half_duplex: false,
        }
    }
}

/// The frames of one response, on their way to the loudspeaker.
#[derive(Clone, Copy, Debug)]
struct Run {
    tag: OpTag,
    /// Its number on the line.
    number: u32,
    /// How many of its samples were put on the line.
    sent: u32,
    /// Whether its last frame was.
    last: bool,
    /// Whether it was discarded.
    cut: bool,
    /// How many of its samples the loudspeaker was last seen to have
    /// played.
    seen: u32,
    /// How many the session was last told were played.
    reported: Option<u32>,
}

/// What stands in for the far end.
#[derive(Debug)]
enum Device {
    /// A sound card. It runs itself, on threads of its own, for as long
    /// as this is kept.
    Card(#[expect(dead_code, reason = "kept so that the streams run")] Card),
    /// A person who follows a script, moved on by the clock whenever the
    /// session looks at the transport.
    Scripted(Person),
}

/// Where a transport is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// The session has not been told that anyone is there.
    New,
    /// The person is there.
    Open,
    /// The person has gone, and the session has been told so.
    Left,
    /// The session has been told that nothing more will happen.
    Closed,
}

/// A session's connection to the person at this machine.
#[derive(Debug)]
pub struct LocalAudio {
    options: Options,
    line: Line,
    device: Device,
    stage: Stage,
    /// The number and the place in time of the next frame of the
    /// microphone's audio.
    sequence: u16,
    timestamp: u32,
    frame: Vec<i16>,
    /// The responses the session has not been told the end of, the
    /// oldest first.
    runs: VecDeque<Run>,
    /// The number of the newest run.
    newest: u32,
    /// Until when the microphone stays deaf although the agent is not
    /// speaking any more.
    deaf_until: Option<Instant>,
}

impl LocalAudio {
    /// A transport on the sound devices of this machine.
    ///
    /// # Errors
    ///
    /// A device is missing, or cannot be run at 48 kHz, or the sound
    /// system refused: see [`DeviceError`].
    pub fn open(choice: &Choice, options: Options) -> Result<Self, DeviceError> {
        let (line, end) = line::line();
        let (line, card) = match card::open(choice, end, false) {
            Ok(card) => (line, card),
            // A device may say that it can work in small pieces and then
            // refuse to. It is asked again, for pieces of its own size.
            Err(DeviceError::System(_)) => {
                let (line, end) = line::line();
                (line, card::open(choice, end, true)?)
            }
            Err(error) => return Err(error),
        };
        Ok(Self::new(line, Device::Card(card), options))
    }

    /// A transport with a person who follows a script in place of the
    /// sound devices, and what that person's loudspeaker will have
    /// played. The script begins when the session first looks at the
    /// transport, and when it has run out the person hangs up.
    #[must_use]
    pub fn scripted(steps: Vec<Step>, options: Options) -> (Self, Recording) {
        let (line, end) = line::line();
        let (person, recording) = Person::new(end, steps);
        let transport = Self::new(line, Device::Scripted(person), options);
        (transport, recording)
    }

    fn new(line: Line, device: Device, options: Options) -> Self {
        Self {
            options,
            line,
            device,
            stage: Stage::New,
            sequence: 0,
            timestamp: 0,
            frame: vec![0; FRAME],
            runs: VecDeque::new(),
            newest: 0,
            deaf_until: None,
        }
    }

    /// A look at how the devices are doing, for whoever started the
    /// session.
    #[must_use]
    pub fn health(&self) -> Health {
        self.line.health()
    }

    /// Whether audio of a response is still to be played, as far as is
    /// known.
    fn speaking(&self, playing: Progress) -> bool {
        self.runs.back().is_some_and(|run| {
            if run.cut {
                return self.line.cut(run.number).is_none();
            }
            playing.run < run.number || (playing.run == run.number && playing.played < run.sent)
        })
    }

    /// What the loudspeaker has played that the session has not been
    /// told.
    fn report(&mut self) -> Option<TransportEvent> {
        let playing = self.line.playing();
        loop {
            let run = self.runs.front_mut()?;
            if playing.run == run.number {
                run.seen = run.seen.max(playing.played);
            }
            let (played, over) = if run.cut {
                match self.line.cut(run.number)? {
                    Cut::At(played) => (played, true),
                    Cut::Passed => (run.seen, true),
                }
            } else {
                match playing.run.cmp(&run.number) {
                    // Not begun.
                    core::cmp::Ordering::Less => return None,
                    core::cmp::Ordering::Equal => {
                        (playing.played, run.last && playing.played >= run.sent)
                    }
                    // The loudspeaker has gone on to a later response.
                    core::cmp::Ordering::Greater => (run.sent, true),
                }
            };
            let played = played.min(run.sent);
            let news = run.reported.is_none_or(|reported| played > reported);
            let step = played.saturating_sub(run.reported.unwrap_or(0));
            // The beginning is said at once, and so is the end, and so is
            // the end of what has come so far of a response that is late:
            // the session waits for those. In between, every so often.
            let caught_up = played >= run.sent;
            let first = run.reported.is_none();
            let due = news && (over || first || caught_up || step >= REPORT_EVERY);
            let tag = run.tag;
            if due {
                run.reported = Some(played);
            }
            if over {
                self.runs.pop_front();
            }
            if due {
                return Some(TransportEvent::Played {
                    tag,
                    played: MediaDur::from_ticks(played),
                });
            }
            if !over {
                return None;
            }
        }
    }

    /// The next frame of what the microphone heard, if there is one.
    fn hear(&mut self) -> Option<TransportEvent> {
        // What was lost for want of room was lost from the stream: the
        // frames after it are later by as much.
        let lost = self.line.lost();
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the place of a packet in time wraps round"
        )]
        let skipped = lost as u32;
        self.timestamp = self.timestamp.wrapping_add(skipped);

        if !self.line.hear(&mut self.frame) {
            return None;
        }
        if self.options.half_duplex {
            let now = Instant::now();
            if self.speaking(self.line.playing()) {
                self.deaf_until = Some(now + DEAF_AFTER);
            }
            if self.deaf_until.is_some_and(|until| now < until) {
                self.frame.fill(0);
            } else {
                self.deaf_until = None;
            }
        }
        let mut payload = Vec::with_capacity(FRAME * 2);
        for sample in &self.frame {
            payload.extend_from_slice(&sample.to_le_bytes());
        }
        let event = TransportEvent::Audio {
            who: self.options.who,
            sequence: self.sequence,
            timestamp: self.timestamp,
            payload: Bytes::from(payload),
        };
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(FRAME_TICKS);
        Some(event)
    }

    /// What has happened that the session has not been told, the
    /// loudspeaker's news before the microphone's.
    fn news(&mut self) -> Option<TransportEvent> {
        self.report().or_else(|| self.hear())
    }
}

impl Transport for LocalAudio {
    fn caps(&self) -> TransportCaps {
        TransportCaps {
            inbound: Codec::Pcm(RATE),
            outbound: Codec::Pcm(RATE),
            remote_buffer: RemoteBuffer {
                clearable: true,
                reports_played: true,
            },
            text: false,
        }
    }

    fn poll_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TransportEvent>> {
        let who = self.options.who;
        match self.stage {
            Stage::New => {
                self.stage = Stage::Open;
                return Poll::Ready(Some(TransportEvent::Joined { who }));
            }
            Stage::Open => {}
            Stage::Left | Stage::Closed => {
                self.stage = Stage::Closed;
                return Poll::Ready(None);
            }
        }
        loop {
            if self.line.failed() {
                self.stage = Stage::Left;
                return Poll::Ready(Some(TransportEvent::Left { who }));
            }
            let moved = match &mut self.device {
                Device::Card(_) => false,
                Device::Scripted(person) => person.poll(cx),
            };
            if let Some(event) = self.news() {
                return Poll::Ready(Some(event));
            }
            match &self.device {
                Device::Scripted(person) if person.gone() => {
                    self.stage = Stage::Left;
                    return Poll::Ready(Some(TransportEvent::Left { who }));
                }
                Device::Scripted(_) if moved => {}
                Device::Scripted(_) => return Poll::Pending,
                Device::Card(_) => {
                    self.line.wake_me(cx.waker());
                    // The card may have had something to say while the
                    // waker was being left for it.
                    return match self.news() {
                        Some(event) => Poll::Ready(Some(event)),
                        None if self.line.failed() => {
                            self.stage = Stage::Left;
                            Poll::Ready(Some(TransportEvent::Left { who }))
                        }
                        None => Poll::Pending,
                    };
                }
            }
        }
    }

    fn try_send(&mut self, frame: FrameOut<'_>) -> Result<(), Full> {
        let (pairs, odd) = frame.payload.as_chunks::<2>();
        // Not audio as this transport takes it. It is let go of: sent
        // again it would be no better.
        if pairs.is_empty() || !odd.is_empty() || self.stage != Stage::Open {
            return Ok(());
        }
        let number = match self.runs.back() {
            // The port allows no frame of a response after the request to
            // discard it.
            Some(run) if run.tag == frame.tag && run.cut => return Ok(()),
            Some(run) if run.tag == frame.tag => run.number,
            _ => self.newest.wrapping_add(1),
        };
        if !self.line.has_room(pairs.len()) {
            return Err(Full);
        }
        let samples = pairs.iter().map(|pair| i16::from_le_bytes(*pair));
        if !self.line.play(number, samples) {
            return Err(Full);
        }
        if number != self.newest {
            self.newest = number;
            if self.runs.len() >= MOST_RUNS {
                self.runs.pop_front();
            }
            self.runs.push_back(Run {
                tag: frame.tag,
                number,
                sent: 0,
                last: false,
                cut: false,
                seen: 0,
                reported: None,
            });
        }
        if let Some(run) = self.runs.back_mut() {
            let length = u32::try_from(pairs.len()).unwrap_or(u32::MAX);
            run.sent = run.sent.saturating_add(length);
            if frame.last {
                run.last = true;
                self.line.end(number);
            }
        }
        Ok(())
    }

    fn try_control(&mut self, control: TransportControl) -> Result<(), Full> {
        let TransportControl::Clear { tag } = control;
        let Some(number) = self
            .runs
            .iter()
            .rev()
            .find(|run| run.tag == tag)
            .map(|run| run.number)
        else {
            return Ok(());
        };
        // What is on the line before it goes with it: the loudspeaker
        // plays in order, and cannot let go of the later and keep the
        // earlier.
        for run in &mut self.runs {
            if run.number <= number {
                run.cut = true;
            }
        }
        self.line.discard(number);
        Ok(())
    }
}
