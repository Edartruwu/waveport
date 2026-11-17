//! A call that a carrier streams, as the transport of a session.

use bytes::Bytes;
use core::task::{Context, Poll};
use core::time::Duration;
use tokio::time::Instant;
use waveport_adapter_kit::line::{Inbound, Packet, Runs};
use waveport_adapter_kit::stream::{self, Commands, Either, Events, Receiver, Sender};
use waveport_adapter_kit::ws::{WsConn, WsMessage};
use waveport_domain::ids::{OpTag, ParticipantId};
use waveport_ports::audio::{Codec, Rate};
use waveport_ports::error::{Full, PortError};
use waveport_ports::transport::{
    AudioCaps, FrameOut, RemoteBuffer, ToneOut, Transport, TransportCaps, TransportControl,
    TransportEvent,
};

use crate::{Carrier, Heard, Started};

/// How long a carrier that connected may take to say which call its
/// stream is of.
pub const STARTS_WITHIN: Duration = Duration::from_secs(5);

/// How many frames and requests may wait for the connection: a second
/// of audio and what goes with it.
const ROOM: usize = 64;

/// How long a carrier is given to close its end of a connection.
const LAST_WORDS: Duration = Duration::from_millis(300);

/// The rate of a telephone.
const RATE: Rate = Rate::Hz8000;

/// The name of the mark after `sent` samples of the response the far
/// end knows as `run`, the last of which ends it: `r3.160` after the
/// first 20 ms of the fourth response, and `r3.480.end` after all of
/// it.
pub(crate) fn mark_name(run: u32, sent: u32, last: bool) -> String {
    let end = if last { ".end" } else { "" };
    format!("r{run}.{sent}{end}")
}

/// What the name of a mark says: the run, how many samples of it came
/// before the mark, and whether it ends the run. `None` for a name that
/// is not one of these.
pub(crate) fn mark_of(name: &str) -> Option<(u32, u32, bool)> {
    let name = name.strip_prefix('r')?;
    let (name, last) = match name.strip_suffix(".end") {
        Some(name) => (name, true),
        None => (name, false),
    };
    let (run, sent) = name.split_once('.')?;
    // Only what `mark_name` writes: a number is its digits.
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !digits(run) || !digits(sent) {
        return None;
    }
    Some((run.parse().ok()?, sent.parse().ok()?, last))
}

/// A call whose carrier has said which one it is, and whose session has
/// not begun.
#[derive(Debug)]
pub struct Call {
    carrier: Carrier,
    conn: WsConn,
    started: Started,
}

impl Call {
    /// Hears the beginning of a carrier's stream on a connection that is
    /// a WebSocket already: one that an HTTP server had and handed over,
    /// or one a listener took.
    ///
    /// # Errors
    ///
    /// The carrier did not say which call the stream is of within
    /// `timeout`, said something that is no message of its stream, or
    /// carries audio that is not taken here. The connection is closed.
    pub async fn of(
        carrier: Carrier,
        mut conn: WsConn,
        timeout: Duration,
    ) -> Result<Self, PortError> {
        let by = Instant::now() + timeout;
        let failed = loop {
            let message = match tokio::time::timeout_at(by, conn.next()).await {
                Ok(Ok(Some(message))) => message,
                Ok(Ok(None) | Err(_)) => {
                    return Err(PortError::transient(
                        "the carrier left before it said which call its stream is of",
                    ));
                }
                Err(_) => {
                    break PortError::timeout(
                        "the carrier did not say which call its stream is of",
                    );
                }
            };
            // A carrier's stream is text. What is not is none of it.
            let WsMessage::Text(text) = message else {
                break PortError::permanent(
                    "a carrier's stream begins with what it is the stream of",
                );
            };
            match carrier.read(text.as_str()) {
                Ok(Heard::Start(started)) => {
                    return Ok(Self {
                        carrier,
                        conn,
                        started,
                    });
                }
                // The stream is there, or something was said that this
                // does not know of: the call is still to come.
                Ok(Heard::Hello | Heard::Other) => {}
                Ok(_) => {
                    break PortError::permanent(
                        "a carrier says which call its stream is of before anything of the call",
                    );
                }
                Err(malformed) => break PortError::permanent(malformed.why()),
            }
        };
        conn.close_and_wait(None, LAST_WORDS).await;
        Err(failed)
    }

    /// The carrier.
    #[must_use]
    pub const fn carrier(&self) -> Carrier {
        self.carrier
    }

    /// What the carrier said as the call's audio began: which call it
    /// is, and what the answer gave the stream to say. Whether that is
    /// good is the server's to judge.
    #[must_use]
    pub const fn started(&self) -> &Started {
        &self.started
    }

    /// The transport of a session for this call. `who` is what the
    /// session knows the caller as.
    #[must_use]
    pub fn transport(self, who: ParticipantId) -> CarrierTransport {
        let Self {
            carrier,
            conn,
            started,
        } = self;
        let (commands, events) = stream::pair(ROOM, move |commands, events| {
            let far = Far {
                who,
                carrier,
                stream: started.stream,
                runs: Runs::default(),
                void_to: None,
                inbound: Inbound::g711(),
            };
            far.serve(conn, commands, events)
        });
        CarrierTransport {
            carrier,
            commands,
            events,
            bidden: false,
            over: false,
        }
    }

    /// Closes the connection of a call that is not served. The carrier
    /// goes on with whatever its instructions say comes next, which for
    /// a call that was only connected to a stream is to end it.
    pub async fn refuse(self) {
        self.conn.close_and_wait(None, LAST_WORDS).await;
    }
}

/// What a session asks of the connection.
enum Command {
    Frame {
        tag: OpTag,
        /// How many samples it holds.
        samples: u32,
        payload: Bytes,
        last: bool,
    },
    Tone {
        payload: Bytes,
    },
    Clear {
        tag: OpTag,
    },
    Close,
}

/// The transport of one session: a call that a carrier streams.
#[derive(Debug)]
pub struct CarrierTransport {
    carrier: Carrier,
    commands: Sender<Command>,
    events: Receiver<TransportEvent>,
    /// Whether the connection has been asked to end the call.
    bidden: bool,
    /// Whether the transport is done with.
    over: bool,
}

impl Transport for CarrierTransport {
    fn caps(&self) -> TransportCaps {
        let codec = Codec::G711(self.carrier.law());
        TransportCaps {
            audio: Some(AudioCaps {
                inbound: codec,
                outbound: codec,
                remote_buffer: RemoteBuffer {
                    clearable: true,
                    reports_played: true,
                },
            }),
            // A telephone does not write.
            text: false,
            keys: Some(self.carrier.keys_out()),
        }
    }

    fn poll_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TransportEvent>> {
        if self.over {
            return Poll::Ready(None);
        }
        match self.events.poll_next(cx) {
            Poll::Ready(Some(Ok(event))) => Poll::Ready(Some(event)),
            Poll::Ready(Some(Err(_)) | None) => {
                self.over = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn try_send(&mut self, frame: FrameOut<'_>) -> Result<(), Full> {
        if self.over {
            return Ok(());
        }
        let samples = RATE.samples_in(frame.length);
        self.commands.try_send(Command::Frame {
            tag: frame.tag,
            samples: u32::try_from(samples).unwrap_or(u32::MAX),
            payload: Bytes::copy_from_slice(frame.payload),
            last: frame.last,
        })
    }

    fn try_control(&mut self, control: TransportControl) -> Result<(), Full> {
        if self.over {
            return Ok(());
        }
        match control {
            TransportControl::Clear { tag } => self.commands.try_send(Command::Clear { tag }),
            // A key is a tone in the audio here, and is not asked for
            // in any other way.
            TransportControl::Dtmf { .. } => Ok(()),
        }
    }

    fn try_send_tone(&mut self, tone: ToneOut<'_>) -> Result<(), Full> {
        if self.over {
            return Ok(());
        }
        self.commands.try_send(Command::Tone {
            payload: Bytes::copy_from_slice(tone.payload),
        })
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            if self.over {
                return Poll::Ready(());
            }
            if !self.bidden {
                self.bidden = self.commands.try_send(Command::Close).is_ok();
            }
            // Running the connection is what ends the call. Whatever the
            // caller says in the meantime is of no interest any more.
            match self.events.poll_next(cx) {
                Poll::Ready(Some(Ok(_))) => {}
                Poll::Ready(Some(Err(_)) | None) => self.over = true,
                Poll::Pending if self.bidden => return Poll::Pending,
                Poll::Pending => {
                    // The connection has taken what waited for it, so
                    // there is room for the request now.
                    self.bidden = self.commands.try_send(Command::Close).is_ok();
                    if !self.bidden {
                        return Poll::Pending;
                    }
                }
            }
        }
    }
}

/// How a call came to its end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// The session ended it.
    Closed,
    /// The caller hung up, or the carrier is gone.
    Left,
}

/// The far end of a call, and what is known of it.
struct Far {
    who: ParticipantId,
    carrier: Carrier,
    /// The carrier's name for the stream, which every message to it
    /// names.
    stream: String,
    runs: Runs,
    /// The newest run the carrier was asked to discard, by the number
    /// it knows it by. A carrier that discards says back every mark it
    /// throws away, and those are of audio nobody heard: no mark of
    /// that run or of one before it is believed from then on.
    void_to: Option<u32>,
    inbound: Inbound,
}

impl Far {
    /// Runs the call for as long as there is a session and a caller.
    async fn serve(
        mut self,
        mut conn: WsConn,
        mut commands: Commands<Command>,
        mut events: Events<TransportEvent>,
    ) -> Result<(), PortError> {
        let who = self.who;
        // The carrier said that the audio of the call begins before
        // there was a transport: the caller is there.
        events.send(TransportEvent::Joined { who }).await;
        let end = loop {
            match stream::first(commands.next(), conn.next()).await {
                // The transport is gone, or the session is over.
                Either::Left(None | Some(Command::Close)) => break End::Closed,
                Either::Left(Some(command)) => {
                    if self.carry_out(&mut conn, command).await.is_err() {
                        break End::Left;
                    }
                }
                Either::Right(Ok(Some(WsMessage::Text(text)))) => {
                    if self.hear(text.as_str(), &mut events).await {
                        break End::Left;
                    }
                }
                // A carrier's stream is text. What is not is none of it.
                Either::Right(Ok(Some(WsMessage::Binary(_)))) => {}
                Either::Right(Ok(None) | Err(_)) => break End::Left,
            }
        };
        // A carrier whose stream is closed goes on with what follows in
        // its instructions, which for a call that was only connected to
        // a stream is to end it.
        conn.close_and_wait(None, LAST_WORDS).await;
        if end == End::Left {
            events.send(TransportEvent::Left { who }).await;
        }
        Ok(())
    }

    /// Does what the session asked.
    async fn carry_out(&mut self, conn: &mut WsConn, command: Command) -> Result<(), PortError> {
        match command {
            Command::Frame {
                tag,
                samples,
                payload,
                last,
            } => {
                let run = self.runs.of(tag);
                let (number, before) = (run.number, run.sent);
                run.sent = before.saturating_add(samples);
                let sent = run.sent;
                // A carrier says a mark back when the caller has been
                // played everything before it. One where a response
                // begins says when the caller is about to hear it: until
                // then nothing of it was heard, whatever the clock says.
                if self.runs.begins(number) {
                    let begins = mark_name(number, before, false);
                    let begins = self.carrier.mark(&self.stream, &begins);
                    conn.send_text(begins).await?;
                }
                if !payload.is_empty() {
                    let media = self.carrier.media(&self.stream, &payload);
                    conn.send_text(media).await?;
                }
                // And one after every frame says how far the caller has
                // heard it, to within that frame.
                let after = mark_name(number, sent, last);
                conn.send_text(self.carrier.mark(&self.stream, &after))
                    .await
            }
            // It is of no response: nothing is to be said back for it.
            Command::Tone { payload } => {
                if payload.is_empty() {
                    return Ok(());
                }
                let media = self.carrier.media(&self.stream, &payload);
                conn.send_text(media).await
            }
            Command::Clear { tag } => {
                // A response of which nothing was sent has nothing to
                // discard.
                let Some(run) = self.runs.tagged(tag) else {
                    return Ok(());
                };
                // The carrier lets go of everything it holds, and so of
                // what was left of a response before this one.
                self.void_to = Some(run.number);
                conn.send_text(self.carrier.clear(&self.stream)).await
            }
            // It ends the call, and is taken before this.
            Command::Close => Ok(()),
        }
    }

    /// Takes what the carrier said. Returns whether it said that the
    /// call is over.
    async fn hear(&mut self, text: &str, events: &mut Events<TransportEvent>) -> bool {
        let who = self.who;
        match self.carrier.read(text) {
            Ok(Heard::Audio(audio)) => {
                // Audio that cannot be cut into packets is passed over:
                // there is nobody to tell that it was not taken.
                if self.inbound.take(&audio, Instant::now()).is_ok() {
                    while let Some(packet) = self.inbound.next_packet() {
                        events.send(audio_of(who, packet)).await;
                    }
                }
            }
            Ok(Heard::Key(key)) => events.send(TransportEvent::Dtmf { who, key }).await,
            Ok(Heard::Mark(name)) => {
                if let Some(played) = self.played(&name) {
                    events.send(played).await;
                }
            }
            Ok(Heard::Stop) => return true,
            // What is said twice, what is not known, and what cannot be
            // read are passed over, and the call goes on.
            Ok(Heard::Hello | Heard::Start(_) | Heard::Other) | Err(_) => {}
        }
        false
    }

    /// What a mark that the carrier said back tells of what was played.
    fn played(&self, name: &str) -> Option<TransportEvent> {
        let (number, sent, _) = mark_of(name)?;
        // A run the carrier was never sent, or one of long ago, is
        // nothing a session can be told about. One that was discarded
        // has its marks said back for audio that was thrown away.
        let run = self.runs.numbered(number)?;
        if self
            .void_to
            .is_some_and(|newest| !comes_after(number, newest))
        {
            return None;
        }
        // No more can have been played than was sent.
        let samples = sent.min(run.sent) as usize;
        Some(TransportEvent::Played {
            tag: run.tag,
            played: RATE.length_of(samples),
        })
    }
}

/// Whether the run `number` was begun after the run `other`. The
/// numbers of runs wrap round.
const fn comes_after(number: u32, other: u32) -> bool {
    let ahead = number.wrapping_sub(other);
    ahead != 0 && ahead < 1 << 31
}

/// A packet of the caller's audio, as the session is told of it.
fn audio_of(who: ParticipantId, packet: Packet) -> TransportEvent {
    let Packet {
        sequence,
        timestamp,
        payload,
    } = packet;
    TransportEvent::Audio {
        who,
        sequence,
        timestamp,
        payload,
    }
}

#[cfg(test)]
mod tests {
    use super::{comes_after, mark_name, mark_of};

    #[test]
    fn a_mark_is_named_by_its_run_and_by_how_much_of_it_came_before() {
        assert_eq!(mark_name(3, 160, false), "r3.160");
        assert_eq!(mark_name(0, 57_600, true), "r0.57600.end");
        for (run, sent, last) in [(0, 0, false), (7, 160, false), (u32::MAX, u32::MAX, true)] {
            assert_eq!(
                mark_of(&mark_name(run, sent, last)),
                Some((run, sent, last))
            );
        }
        // A carrier says back what it was sent. Anything else is no
        // mark of a session's.
        for none in [
            "",
            "r",
            "r3",
            "r3.",
            "r.160",
            "3.160",
            "r3.160.",
            "r3.160.fin",
            "r-1.160",
            "r3.+160",
            "r3.160.end.end",
            "r 3.160",
            "my label",
            "r99999999999.1",
        ] {
            assert_eq!(mark_of(none), None, "{none:?}");
        }
    }

    #[test]
    fn a_run_comes_after_the_ones_that_were_begun_before_it() {
        assert!(comes_after(1, 0));
        assert!(comes_after(70, 3));
        assert!(!comes_after(3, 3));
        assert!(!comes_after(2, 3));
        assert!(!comes_after(0, 70));
        // The numbers wrap round.
        assert!(comes_after(0, u32::MAX));
        assert!(comes_after(5, u32::MAX - 5));
        assert!(!comes_after(u32::MAX, 0));
    }
}
