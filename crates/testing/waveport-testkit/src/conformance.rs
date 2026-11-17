//! The rules every adapter of a port has to keep.
//!
//! A port's traits say what an adapter can be asked. They cannot say in
//! what order its events come, what its marks may point at, or that a
//! stream which is over stays over. Those rules are written here, once,
//! as checks that drive any adapter through a small piece of work and
//! say which rule it broke, if it broke one.
//!
//! The checks know nothing about what is behind the adapter. They run
//! against the fakes of this crate, against an adapter that talks to a
//! server replaying what a provider once sent, and against an adapter
//! that talks to the provider itself.
//!
//! A check that passes gives back what it saw, so that the caller can go
//! on to check what only it knows: the words that were spoken, the tool
//! that should have been called.

use core::fmt;
use core::future::poll_fn;
use core::pin::{Pin, pin};
use core::task::{Context, Poll};
use core::time::Duration;
use std::collections::VecDeque;

use bytes::Bytes;
use waveport_domain::event::Key;
use waveport_domain::ids::{Epoch, GenId, OpTag, ParticipantId};
use waveport_domain::time::{MediaDur, MediaSpan, MediaTime};
use waveport_ports::audio::{AudioRef, Codec, Rate};
use waveport_ports::error::PortError;
use waveport_ports::journal::{Batch, Journal, JournalError, Seq, SessionId, Snapshot};
use waveport_ports::llm::{Finish, Llm, LlmEvent, LlmRequest, LlmRx, Usage};
use waveport_ports::stt::{Stt, SttCaps, SttConfig, SttEvent, SttFinalize, SttRx, Transcript};
use waveport_ports::transport::{AudioCaps, FrameOut, Transport, TransportControl, TransportEvent};
use waveport_ports::tts::{Alignment, Tts, TtsConfig, TtsEvent, TtsMark};

use crate::drive::{next_llm, next_stt, next_tts};

/// How long a check waits for an adapter to give its next event before
/// it calls the adapter stuck.
pub const PATIENCE: Duration = Duration::from_secs(30);

/// How far outside the audio that was sent the span of a transcript may
/// reach: providers round their times.
const SPAN_SLACK: MediaDur = MediaDur::from_millis(300);

/// A rule an adapter broke.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Broken(pub String);

impl fmt::Display for Broken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl core::error::Error for Broken {}

fn broken<T>(rule: impl Into<String>) -> Result<T, Broken> {
    Err(Broken(rule.into()))
}

/// Waits for `next`, but not for ever.
async fn patiently<T>(what: &str, next: impl Future<Output = T>) -> Result<T, Broken> {
    match tokio::time::timeout(PATIENCE, next).await {
        Ok(value) => Ok(value),
        Err(_) => broken(format!(
            "{what} did not arrive within {} s",
            PATIENCE.as_secs()
        )),
    }
}

// ---------------------------------------------------------------------
// Language models
// ---------------------------------------------------------------------

/// What a language model wrote in one response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LlmSeen {
    /// The text, put together.
    pub text: String,
    /// The tool calls it asked for: the tool's name and its arguments.
    pub calls: Vec<(String, String)>,
    /// Why it stopped.
    pub finish: Finish,
    /// What it said the response cost.
    pub usage: Option<Usage>,
}

/// Asks a language model for one response and reads it to its end.
///
/// The rules: the response starts; no piece of text is empty; tool calls
/// are asked for only by a model that says it can, only for tools that
/// were offered, and one at most by a model that says it cannot ask for
/// several; a response ends by saying why, and says that it waits for
/// tool calls exactly when it asked for some; after that the stream is
/// over, and stays over.
///
/// # Errors
///
/// The rule the adapter broke.
pub async fn llm_response(llm: &dyn Llm, request: LlmRequest<'_>) -> Result<LlmSeen, Broken> {
    let caps = llm.caps();
    let mut rx = match patiently("the start of the response", llm.start(request)).await? {
        Ok(rx) => rx,
        Err(error) => return broken(format!("the response did not start: {error}")),
    };
    let mut text = String::new();
    let mut calls = Vec::new();
    let (finish, usage) = loop {
        match patiently("the next piece of the response", next_llm(&mut rx)).await? {
            Some(Ok(LlmEvent::Delta { text: piece })) => {
                if piece.is_empty() {
                    return broken("a piece of text is empty");
                }
                text.push_str(&piece);
            }
            Some(Ok(LlmEvent::ToolCall { tool, args })) => {
                if !caps.tools {
                    return broken("a model that says it cannot call tools asked for a call");
                }
                if !request.tools.iter().any(|offered| offered.name == tool) {
                    return broken(format!("a call asks for `{tool}`, which was not offered"));
                }
                calls.push((tool, args));
            }
            Some(Ok(LlmEvent::Done { finish, usage })) => break (finish, usage),
            Some(Err(error)) => return broken(format!("the response failed: {error}")),
            None => return broken("the response ended without saying why it stopped"),
        }
    };
    if calls.len() > 1 && !caps.parallel_tools {
        return broken("a model that says it asks for one call at a time asked for several");
    }
    if calls.is_empty() == (finish == Finish::ToolCalls) {
        return broken(format!(
            "the response asked for {} tool calls and stopped with {finish:?}",
            calls.len()
        ));
    }
    llm_is_over(&mut rx).await?;
    Ok(LlmSeen {
        text,
        calls,
        finish,
        usage,
    })
}

async fn llm_is_over(rx: &mut Pin<Box<dyn LlmRx>>) -> Result<(), Broken> {
    for _ in 0..2 {
        if patiently("the end of the response", next_llm(rx))
            .await?
            .is_some()
        {
            return broken("something followed the end of the response");
        }
    }
    Ok(())
}

/// Starts a response, reads its first event, abandons it, and then asks
/// for another and reads that to its end: a model that is cut off has to
/// be as good as new.
///
/// # Errors
///
/// The rule the adapter broke.
pub async fn llm_abandoned(llm: &dyn Llm, request: LlmRequest<'_>) -> Result<LlmSeen, Broken> {
    let mut rx = match patiently("the start of the response", llm.start(request)).await? {
        Ok(rx) => rx,
        Err(error) => return broken(format!("the response did not start: {error}")),
    };
    match patiently("the first piece of the response", next_llm(&mut rx)).await? {
        Some(Ok(_)) => {}
        Some(Err(error)) => return broken(format!("the response failed: {error}")),
        None => return broken("the response ended before it began"),
    }
    drop(rx);
    llm_response(llm, request).await
}

// ---------------------------------------------------------------------
// Speech recognition
// ---------------------------------------------------------------------

/// Audio to recognise: one stretch of speech, with the silence after it
/// that a recogniser which decides for itself needs in order to decide.
#[derive(Clone, Copy, Debug)]
pub struct Talk<'a> {
    /// Where the audio begins on the media clock.
    pub at: MediaTime,
    /// Its rate, which has to be one the recogniser takes.
    pub rate: Rate,
    /// The samples.
    pub samples: &'a [i16],
    /// Whether to send it at the pace it is spoken. A provider needs
    /// that; a fake does not care.
    pub paced: bool,
}

/// What a speech recogniser gave for one stretch of speech.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SttSeen {
    /// The transcripts that were not final, in order.
    pub interim: Vec<Transcript>,
    /// The final transcripts, in order.
    pub finals: Vec<Transcript>,
}

impl SttSeen {
    /// The final transcripts, put together.
    #[must_use]
    pub fn text(&self) -> String {
        self.finals
            .iter()
            .map(|transcript| transcript.text.trim())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Sends a recogniser one stretch of speech and reads what it makes of
/// it.
///
/// The rules: the stream opens at a rate the recogniser says it takes,
/// and takes audio that is sent no faster than it is polled; a final
/// transcript arrives, on request from a recogniser that says it honours
/// one, and by itself from the others; interim transcripts come only
/// from a recogniser that says it gives them; every span lies within the
/// audio that was sent, and final transcripts do not go back in time;
/// once the sending half is dropped the stream comes to its end, and
/// stays over.
///
/// # Errors
///
/// The rule the adapter broke.
pub async fn stt_utterance(stt: &dyn Stt, talk: Talk<'_>) -> Result<SttSeen, Broken> {
    let caps = stt.caps();
    if !caps.rates.contains(talk.rate) {
        return broken(format!(
            "the check was given audio at {} Hz, which the recogniser does not take",
            talk.rate.hz()
        ));
    }
    let opened = patiently("the stream", stt.open(SttConfig::new(talk.rate))).await?;
    let mut stream = match opened {
        Ok(stream) => stream,
        Err(error) => return broken(format!("the stream did not open: {error}")),
    };
    let sent = MediaSpan::new(
        talk.at,
        talk.at.after(talk.rate.length_of(talk.samples.len())),
    );
    let mut seen = SttSeen::default();

    let mut at = talk.at;
    for samples in talk.samples.chunks(talk.rate.frame()) {
        let piece = AudioRef {
            at,
            rate: talk.rate,
            samples,
        };
        if stream.tx.try_send(piece).is_err() {
            return broken("audio was refused although the stream was polled after every piece");
        }
        at = at.after(piece.length());
        if talk.paced {
            tokio::time::sleep(Duration::from_millis(20)).await;
        } else {
            // Whatever else runs beside the check gets its turn: a server
            // on this machine that the adapter talks to, for one.
            tokio::task::yield_now().await;
        }
        for event in stt_ready(&mut stream.rx).await? {
            stt_note(event, caps, sent, &mut seen)?;
        }
    }

    if caps.finalize.on_request() && stream.tx.try_finalize().is_err() {
        return broken("the request for a final transcript was refused");
    }
    let asked = seen.finals.len();
    while seen.finals.len() == asked {
        let what = if caps.finalize == SttFinalize::ByProvider {
            "a final transcript, which the recogniser gives by itself,"
        } else {
            "the final transcript that was asked for"
        };
        match patiently(what, next_stt(&mut stream.rx)).await? {
            Some(Ok(event)) => stt_note(event, caps, sent, &mut seen)?,
            Some(Err(error)) => return broken(format!("the stream failed: {error}")),
            None => return broken("the stream ended without a final transcript"),
        }
    }

    drop(stream.tx);
    loop {
        match patiently("the end of the stream", next_stt(&mut stream.rx)).await? {
            Some(Ok(event)) => stt_note(event, caps, sent, &mut seen)?,
            Some(Err(error)) => return broken(format!("the stream failed: {error}")),
            None => break,
        }
    }
    if patiently("the end of the stream", next_stt(&mut stream.rx))
        .await?
        .is_some()
    {
        return broken("something followed the end of the stream");
    }
    Ok(seen)
}

/// The events a stream has ready now.
async fn stt_ready(rx: &mut Pin<Box<dyn SttRx>>) -> Result<Vec<SttEvent>, Broken> {
    let mut ready = Vec::new();
    let failed = poll_fn(|cx| {
        loop {
            match rx.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(event))) => ready.push(event),
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Some(error)),
                Poll::Ready(None) | Poll::Pending => return Poll::Ready(None),
            }
        }
    })
    .await;
    match failed {
        Some(error) => stream_failed(&error),
        None => Ok(ready),
    }
}

fn stream_failed<T>(error: &PortError) -> Result<T, Broken> {
    broken(format!("the stream failed: {error}"))
}

fn stt_note(
    event: SttEvent,
    caps: SttCaps,
    sent: MediaSpan,
    seen: &mut SttSeen,
) -> Result<(), Broken> {
    let SttEvent::Transcript(transcript) = event;
    let span = transcript.span;
    if span.end < span.start {
        return broken(format!("a transcript ends before it begins: {span:?}"));
    }
    let earliest = sent
        .start
        .ticks()
        .saturating_sub(u64::from(SPAN_SLACK.ticks()));
    if span.start.ticks() < earliest || span.end > sent.end.after(SPAN_SLACK) {
        return broken(format!(
            "a transcript is of {span:?}, outside the audio that was sent, {sent:?}"
        ));
    }
    if transcript.is_final {
        if let Some(last) = seen.finals.last()
            && span.start < last.span.start
        {
            return broken("a final transcript begins before the one before it");
        }
        seen.finals.push(transcript);
    } else {
        if !caps.interim {
            return broken("a recogniser that says it gives no interim transcripts gave one");
        }
        seen.interim.push(transcript);
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Speech synthesis
// ---------------------------------------------------------------------

/// What a speech synthesiser gave for one response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TtsSeen {
    /// The audio, put together.
    pub samples: Vec<i16>,
    /// The marks, in order.
    pub marks: Vec<TtsMark>,
    /// How long the audio is.
    pub length: MediaDur,
}

/// Has a synthesiser speak the pieces of one response and reads the
/// audio to its end.
///
/// The rules: the stream opens at a rate the synthesiser says it gives;
/// audio arrives, and no piece of it is empty; marks come in order, point
/// at a place between two characters of the text that was given, and at
/// no more audio than there is in the end; a synthesiser that says it
/// marks words or characters gives marks, and they reach the last word;
/// the stream ends by saying that it is done, and after that it is over
/// and stays over.
///
/// # Errors
///
/// The rule the adapter broke.
pub async fn tts_utterance(tts: &dyn Tts, rate: Rate, pieces: &[&str]) -> Result<TtsSeen, Broken> {
    let caps = tts.caps();
    if !caps.rates.contains(rate) {
        return broken(format!(
            "the check asked for audio at {} Hz, which the synthesiser does not give",
            rate.hz()
        ));
    }
    let opened = patiently("the stream", tts.open(TtsConfig::new(rate))).await?;
    let mut stream = match opened {
        Ok(stream) => stream,
        Err(error) => return broken(format!("the stream did not open: {error}")),
    };
    for piece in pieces {
        if stream.tx.try_speak(piece).is_err() {
            return broken("text was refused");
        }
    }
    if stream.tx.try_finish().is_err() {
        return broken("the end of the text was refused");
    }
    let text: String = pieces.concat();

    let mut samples = Vec::new();
    let mut marks: Vec<TtsMark> = Vec::new();
    loop {
        match patiently("the next piece of audio", next_tts(&mut stream.rx)).await? {
            Some(Ok(TtsEvent::Audio(pcm))) => {
                if pcm.is_empty() {
                    return broken("a piece of audio is empty");
                }
                samples.extend(pcm.samples());
            }
            Some(Ok(TtsEvent::Mark(mark))) => {
                if let Some(last) = marks.last()
                    && (mark.text_end < last.text_end || mark.audio_end < last.audio_end)
                {
                    return broken(format!("a mark goes back: {mark:?} after {last:?}"));
                }
                let at = usize::try_from(mark.text_end).unwrap_or(usize::MAX);
                if !text.is_char_boundary(at) {
                    return broken(format!(
                        "a mark points at byte {at} of the text, which is not between two characters of it"
                    ));
                }
                marks.push(mark);
            }
            Some(Ok(TtsEvent::Done)) => break,
            Some(Err(error)) => return broken(format!("the stream failed: {error}")),
            None => return broken("the stream ended without saying that it was done"),
        }
    }
    for _ in 0..2 {
        if patiently("the end of the stream", next_tts(&mut stream.rx))
            .await?
            .is_some()
        {
            return broken("something followed the end of the stream");
        }
    }

    if samples.is_empty() {
        return broken("no audio arrived");
    }
    let length = rate.length_of(samples.len());
    if let Some(mark) = marks.iter().find(|mark| mark.audio_end > length) {
        return broken(format!(
            "a mark points at {} ms of audio, and there are {} ms",
            mark.audio_end.as_millis(),
            length.as_millis()
        ));
    }
    if caps.alignment != Alignment::None {
        // Where the last word of the text begins.
        let spoken = text.trim_end();
        let last_word = spoken
            .rfind(char::is_whitespace)
            .map_or(0, |space| space + 1);
        let reached = marks
            .last()
            .map_or(0, |mark| usize::try_from(mark.text_end).unwrap_or(0));
        if reached <= last_word {
            return broken(format!(
                "a synthesiser that says it marks words gave marks up to byte {reached}, \
                 and the last word begins at byte {last_word}"
            ));
        }
    }
    Ok(TtsSeen {
        samples,
        marks,
        length,
    })
}

/// Has a synthesiser start on a response, abandons it at its first
/// audio, and then has it speak another to its end: a synthesiser that
/// is cut off has to be as good as new.
///
/// # Errors
///
/// The rule the adapter broke.
pub async fn tts_abandoned(tts: &dyn Tts, rate: Rate, pieces: &[&str]) -> Result<TtsSeen, Broken> {
    let opened = patiently("the stream", tts.open(TtsConfig::new(rate))).await?;
    let mut stream = match opened {
        Ok(stream) => stream,
        Err(error) => return broken(format!("the stream did not open: {error}")),
    };
    for piece in pieces {
        if stream.tx.try_speak(piece).is_err() {
            return broken("text was refused");
        }
    }
    loop {
        match patiently("the first audio", next_tts(&mut stream.rx)).await? {
            Some(Ok(TtsEvent::Audio(_))) => break,
            Some(Ok(_)) => {}
            Some(Err(error)) => return broken(format!("the stream failed: {error}")),
            None => return broken("the stream ended before any audio"),
        }
    }
    drop(stream.rx);
    // Text for a stream that is gone goes nowhere, and must do no harm.
    let _ = stream.tx.try_speak("and this is never heard.");
    drop(stream.tx);
    tts_utterance(tts, rate, pieces).await
}

// ---------------------------------------------------------------------
// Transports
// ---------------------------------------------------------------------

/// How long one piece of audio is that a check sends, either way.
const PIECE: MediaDur = MediaDur::from_millis(20);

/// Something the far end of a transport is sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FarHeard {
    /// The audio that follows is that of a run, which the far end knows
    /// by this number.
    Begins(u32),
    /// Audio of the run that began last, as the session sent it.
    Audio(Bytes),
    /// All of a run has been sent.
    Ends(u32),
    /// What the far end holds of a run and has not played is to be
    /// discarded.
    Clear(u32),
    /// The session is over. Nothing follows.
    Bye,
}

/// The far end of a transport, as a check has it act. A transport's own
/// tests have a client for it, and that client is one of these.
///
/// A check waits for each of these while it looks at the transport as a
/// session does: a transport that does its work when it is looked at
/// has done it by the time the far end is answered.
pub trait FarEnd {
    /// Says 20 ms of something, as the line carries audio, and returns
    /// the bytes as they were sent. No two pieces are alike: `n` tells
    /// them apart.
    fn speak(&mut self, n: u16) -> impl Future<Output = Result<Bytes, PortError>>;

    /// Says how much of the run `run` has been played so far.
    fn played(&mut self, run: u32, played: MediaDur)
    -> impl Future<Output = Result<(), PortError>>;

    /// Types `text` in place of speech.
    fn write(&mut self, text: &str) -> impl Future<Output = Result<(), PortError>>;

    /// Presses `keys` of a telephone, one after the other. Returns
    /// whether the far end has a way to: one that has none presses
    /// nothing.
    fn press(&mut self, keys: &str) -> impl Future<Output = Result<bool, PortError>>;

    /// The next thing the far end is sent that a check looks at. `None`
    /// when its connection is closed and nothing said that the session
    /// is over.
    fn hear(&mut self) -> impl Future<Output = Result<Option<FarHeard>, PortError>>;

    /// Leaves: says goodbye, as a far end does that ends the session.
    fn leave(&mut self) -> impl Future<Output = Result<(), PortError>>;
}

/// A transport, looked at as a session looks at it.
struct Watched<'a> {
    transport: &'a mut dyn Transport,
    /// What it said that no check has looked at yet.
    said: VecDeque<TransportEvent>,
    /// Whether it said that it is over.
    over: bool,
}

impl<'a> Watched<'a> {
    fn new(transport: &'a mut dyn Transport) -> Self {
        Self {
            transport,
            said: VecDeque::new(),
            over: false,
        }
    }

    fn look(&mut self, cx: &mut Context<'_>) {
        while !self.over {
            match self.transport.poll_event(cx) {
                Poll::Ready(Some(event)) => self.said.push_back(event),
                Poll::Ready(None) => self.over = true,
                Poll::Pending => break,
            }
        }
    }

    /// Has the far end do `work` while the transport is looked at.
    async fn during<T>(
        &mut self,
        what: &str,
        work: impl Future<Output = Result<T, PortError>>,
    ) -> Result<T, Broken> {
        let mut work = pin!(work);
        let both = poll_fn(|cx| {
            self.look(cx);
            work.as_mut().poll(cx)
        });
        match patiently(what, both).await? {
            Ok(done) => Ok(done),
            Err(error) => broken(format!("the far end failed at {what}: {error}")),
        }
    }

    /// The next thing the transport says. `None` once it is over.
    async fn next(&mut self, what: &str) -> Result<Option<TransportEvent>, Broken> {
        let next = poll_fn(|cx| {
            self.look(cx);
            match self.said.pop_front() {
                Some(event) => Poll::Ready(Some(event)),
                None if self.over => Poll::Ready(None),
                None => Poll::Pending,
            }
        });
        patiently(what, next).await
    }

    /// The first thing a transport says: that its far end joined.
    async fn joined(&mut self, who: ParticipantId) -> Result<(), Broken> {
        match self.next("that the far end joined").await? {
            Some(TransportEvent::Joined { who: joined }) if joined == who => Ok(()),
            other => broken(format!(
                "the first thing a transport says is that {who:?} joined: it said {other:?}"
            )),
        }
    }

    /// The next thing the far end is sent.
    async fn far_hears<F: FarEnd>(&mut self, far: &mut F, what: &str) -> Result<FarHeard, Broken> {
        match self.during(what, far.hear()).await? {
            Some(heard) => Ok(heard),
            None => broken(format!(
                "the far end was sent nothing more when {what} was due"
            )),
        }
    }

    /// Sends one piece of a run, every byte of it `fill`. Returns what
    /// was sent.
    fn send(
        &mut self,
        audio: AudioCaps,
        tag: OpTag,
        at: u32,
        fill: u8,
        last: bool,
    ) -> Result<Vec<u8>, Broken> {
        let payload = match audio.outbound {
            Codec::Pcm(rate) => vec![fill; rate.samples_in(PIECE) * 2],
            Codec::G711(_) => vec![fill; Rate::Hz8000.samples_in(PIECE)],
            // A packet of 20 ms, as its first byte says. What follows is
            // the packet's own business.
            Codec::Opus => vec![0xF8, fill, fill],
        };
        let frame = FrameOut {
            tag,
            at: MediaDur::from_millis(at * 20),
            length: PIECE,
            payload: &payload,
            last,
        };
        if self.transport.try_send(frame).is_err() {
            return broken("a transport has room for a few frames of a response");
        }
        Ok(payload)
    }

    /// What the far end hears of one run, from where it begins to
    /// `ends`. Returns the number the far end knows the run by, and its
    /// audio.
    async fn far_hears_run<F: FarEnd>(
        &mut self,
        far: &mut F,
        ends: impl Fn(u32) -> FarHeard,
    ) -> Result<(u32, Vec<u8>), Broken> {
        let run = match self.far_hears(far, "where a run begins").await? {
            FarHeard::Begins(run) => run,
            other => {
                return broken(format!(
                    "the far end is told whose audio follows before it is sent any: it was \
                     sent {other:?}"
                ));
            }
        };
        let mut audio = Vec::new();
        loop {
            match self.far_hears(far, "the audio of a run").await? {
                FarHeard::Audio(piece) => audio.extend_from_slice(&piece),
                heard if heard == ends(run) => return Ok((run, audio)),
                other => {
                    return broken(format!(
                        "the audio of run {run} is followed by {:?}: the far end was sent \
                         {other:?}",
                        ends(run)
                    ));
                }
            }
        }
    }

    /// The transport is over: it says so every time it is asked, and
    /// takes what it is given without a word.
    async fn is_over(&mut self, audio: Option<AudioCaps>) -> Result<(), Broken> {
        for _ in 0..2 {
            let again = poll_fn(|cx| self.transport.poll_event(cx));
            match patiently("a transport that is over, asked again", again).await? {
                None => {}
                Some(event) => {
                    return broken(format!(
                        "a transport that is over says nothing more: it said {event:?}"
                    ));
                }
            }
        }
        if let Some(audio) = audio {
            let _ = self.send(audio, tag(90), 0, 1, true);
        }
        let _ = self
            .transport
            .try_control(TransportControl::Clear { tag: tag(90) });
        let closed = poll_fn(|cx| self.transport.poll_close(cx));
        patiently("the goodbye of a transport that is over", closed).await
    }
}

/// What a check calls a response.
const fn tag(generation: u32) -> OpTag {
    OpTag {
        generation: GenId(generation),
        epoch: Epoch(1),
    }
}

/// Has the far end of a transport speak, type and press keys, and looks
/// at what the transport makes of it. `transport` has to be one that
/// nobody has looked at yet, and `far` its far end, known to it as
/// `who`.
///
/// The rules: the first thing a transport says is that its far end
/// joined; what the far end says arrives as packets numbered one after
/// the other, each placed on the clock of its codec where the one
/// before it ended, and holding together what was sent; what is typed
/// arrives as it was typed, on a transport that says it carries text;
/// and keys arrive one by one, in the order they were pressed.
///
/// # Errors
///
/// The rule the adapter broke.
pub async fn transport_hears<F: FarEnd>(
    transport: &mut dyn Transport,
    far: &mut F,
    who: ParticipantId,
) -> Result<(), Broken> {
    let caps = transport.caps();
    let mut line = Watched::new(transport);
    line.joined(who).await?;

    if let Some(audio) = caps.audio {
        let mut sent = Vec::new();
        for n in 0..5 {
            let piece = line.during("saying something", far.speak(n)).await?;
            sent.extend_from_slice(&piece);
        }
        let mut heard = Vec::new();
        // The number and the place the next packet has to have.
        let mut due: Option<(u16, u32)> = None;
        while heard.len() < sent.len() {
            let event = line.next("what the far end said").await?;
            let Some(TransportEvent::Audio {
                who: whose,
                sequence,
                timestamp,
                payload,
            }) = event
            else {
                return broken(format!(
                    "what the far end says arrives as audio: the transport said {event:?}"
                ));
            };
            if whose != who {
                return broken(format!(
                    "the audio of {who:?} was said to be that of {whose:?}"
                ));
            }
            if payload.is_empty() {
                return broken("a packet of audio holds some");
            }
            if let Some((number, place)) = due
                && (sequence, timestamp) != (number, place)
            {
                return broken(format!(
                    "packets are numbered one after the other and placed where the one before \
                     ended: after one that made {number} at {place} due came {sequence} at \
                     {timestamp}"
                ));
            }
            let held = match audio.inbound {
                Codec::Pcm(_) => payload.len() / 2,
                Codec::G711(_) => payload.len(),
                // A packet a piece: the far end says 20 ms at a time.
                Codec::Opus => Rate::Hz48000.samples_in(PIECE),
            };
            let held = u32::try_from(held).unwrap_or(u32::MAX);
            due = Some((sequence.wrapping_add(1), timestamp.wrapping_add(held)));
            heard.extend_from_slice(&payload);
        }
        if heard != sent {
            return broken("the packets hold together what the far end sent, and nothing else");
        }
    }

    if caps.text {
        let typed = "What time is it?";
        line.during("typing", far.write(typed)).await?;
        let event = line.next("what the far end typed").await?;
        let arrived = matches!(
            &event,
            Some(TransportEvent::Text { who: whose, text }) if *whose == who && text == typed
        );
        if !arrived {
            return broken(format!(
                "what is typed arrives as it was typed: {typed:?} arrived as {event:?}"
            ));
        }
    }

    if line.during("pressing keys", far.press("7#")).await? {
        for key in [Key::Seven, Key::Hash] {
            let event = line.next("a key the far end pressed").await?;
            if event != Some(TransportEvent::Dtmf { who, key }) {
                return broken(format!(
                    "keys arrive one by one, in the order they were pressed: {key:?} arrived \
                     as {event:?}"
                ));
            }
        }
    }
    Ok(())
}

/// Sends the far end of a transport what a session sends, and has it
/// say what it played. `transport` has to be one that nobody has looked
/// at yet, and `far` its far end, known to it as `who`.
///
/// The rules: the far end is told whose audio follows before it is sent
/// any, and when all of a response has been sent; the audio arrives in
/// order and as it was sent; what the far end says it played is told of
/// the response it was the audio of, and is never more than was sent; a
/// report about audio the far end was never sent is told to nobody; a
/// response that is discarded is said to be, unless nothing of it was
/// sent, and the next one is told apart from it. What a transport does
/// not say it can do is not asked of it.
///
/// # Errors
///
/// The rule the adapter broke.
pub async fn transport_speaks<F: FarEnd>(
    transport: &mut dyn Transport,
    far: &mut F,
    who: ParticipantId,
) -> Result<(), Broken> {
    let caps = transport.caps();
    let mut line = Watched::new(transport);
    line.joined(who).await?;
    let Some(audio) = caps.audio else {
        return Ok(());
    };

    // A response of three pieces.
    let mut sent = Vec::new();
    for (at, fill) in [1, 2, 3].into_iter().enumerate() {
        let at = u32::try_from(at).unwrap_or(0);
        sent.extend(line.send(audio, tag(1), at, fill, fill == 3)?);
    }
    let (first, heard) = line.far_hears_run(far, FarHeard::Ends).await?;
    if heard != sent {
        return broken("the audio of a response arrives in order, and as it was sent");
    }

    if audio.remote_buffer.reports_played {
        let told = |played| TransportEvent::Played {
            tag: tag(1),
            played,
        };
        let reports = [
            // A piece of it, more than there was of it, and a run the
            // far end was never sent, which is told to nobody.
            (first, PIECE, Some(PIECE)),
            (
                first,
                MediaDur::from_millis(5_000),
                Some(MediaDur::from_millis(60)),
            ),
            (first.wrapping_add(1_000), PIECE, None),
            (
                first,
                MediaDur::from_millis(40),
                Some(MediaDur::from_millis(40)),
            ),
        ];
        for (run, played, due) in reports {
            line.during("saying what was played", far.played(run, played))
                .await?;
            let Some(due) = due else {
                continue;
            };
            let event = line.next("what the far end played").await?;
            if event != Some(told(due)) {
                return broken(format!(
                    "what the far end played is told of the response it was the audio of, and \
                     is no more than was sent of it: {:?} was due, and the transport said \
                     {event:?}",
                    told(due)
                ));
            }
        }
    }

    // A response nothing was sent of has nothing to discard. One that
    // was begun is discarded, and the one after it begins anew.
    if audio.remote_buffer.clearable {
        for of in [7, 2] {
            if of == 2 {
                line.send(audio, tag(2), 0, 4, false)?;
            }
            let discard = TransportControl::Clear { tag: tag(of) };
            if line.transport.try_control(discard).is_err() {
                return broken("a transport has room for a request to discard");
            }
        }
        let (second, _) = line.far_hears_run(far, FarHeard::Clear).await?;
        if second == first {
            return broken("two responses are not known to the far end by one number");
        }
    }
    let last = line.send(audio, tag(3), 0, 5, true)?;
    let (third, heard) = line.far_hears_run(far, FarHeard::Ends).await?;
    if third == first {
        return broken("two responses are not known to the far end by one number");
    }
    if heard != last {
        return broken("the audio of a response arrives in order, and as it was sent");
    }
    Ok(())
}

/// Ends a session on a transport: sends a last response, and has the
/// transport say goodbye. `transport` has to be one that nobody has
/// looked at yet, and `far` its far end, known to it as `who`.
///
/// The rules: what was sent before the goodbye arrives before it; the
/// far end is told that the session is over, or its connection is
/// closed; saying goodbye comes to an end; and a transport that is over
/// says so every time it is asked, and takes what it is given without a
/// word.
///
/// # Errors
///
/// The rule the adapter broke.
pub async fn transport_says_goodbye<F: FarEnd>(
    transport: &mut dyn Transport,
    far: &mut F,
    who: ParticipantId,
) -> Result<(), Broken> {
    let caps = transport.caps();
    let mut line = Watched::new(transport);
    line.joined(who).await?;
    let sent = match caps.audio {
        Some(audio) => Some(line.send(audio, tag(1), 0, 6, true)?),
        None => None,
    };

    // The session is over: it looks at its transport no more, and asks
    // it to say goodbye.
    let mut heard = Vec::new();
    let mut ended = false;
    {
        let transport = &mut *line.transport;
        let mut hearing = pin!(async {
            loop {
                match far.hear().await? {
                    Some(FarHeard::Audio(piece)) => heard.extend_from_slice(&piece),
                    Some(FarHeard::Ends(_)) => ended = true,
                    Some(FarHeard::Bye) | None => return Ok::<_, PortError>(()),
                    Some(_) => {}
                }
            }
        });
        // Whether the transport is done saying it, and how the far end
        // came to know.
        let mut said = false;
        let mut known = None;
        let both = poll_fn(|cx| {
            if !said {
                said = transport.poll_close(cx).is_ready();
            }
            if known.is_none()
                && let Poll::Ready(done) = hearing.as_mut().poll(cx)
            {
                known = Some(done);
            }
            match known.take_if(|_| said) {
                Some(done) => Poll::Ready(done),
                None => Poll::Pending,
            }
        });
        if let Err(error) = patiently("a goodbye, said and heard", both).await? {
            return broken(format!("the far end failed at hearing a goodbye: {error}"));
        }
    }
    if let Some(sent) = sent
        && (heard != sent || !ended)
    {
        return broken("what a session sent before it ended arrives before the goodbye");
    }
    line.over = true;
    line.is_over(caps.audio).await
}

/// Has the far end of a transport leave. `transport` has to be one that
/// nobody has looked at yet, and `far` its far end, known to it as
/// `who`.
///
/// The rules: a far end that says goodbye has left, which the transport
/// says once; nothing follows that but the end of the transport; and a
/// transport that is over says so every time it is asked, takes what it
/// is given without a word, and has nobody to say goodbye to.
///
/// # Errors
///
/// The rule the adapter broke.
pub async fn transport_is_left<F: FarEnd>(
    transport: &mut dyn Transport,
    far: &mut F,
    who: ParticipantId,
) -> Result<(), Broken> {
    let caps = transport.caps();
    let mut line = Watched::new(transport);
    line.joined(who).await?;
    line.during("leaving", far.leave()).await?;
    match line.next("that the far end left").await? {
        Some(TransportEvent::Left { who: left }) if left == who => {}
        other => {
            return broken(format!(
                "a far end that says goodbye has left: the transport said {other:?}"
            ));
        }
    }
    if let Some(event) = line.next("the end of a transport").await? {
        return broken(format!(
            "nothing follows a far end that left: the transport said {event:?}"
        ));
    }
    line.is_over(caps.audio).await
}

// ---------------------------------------------------------------------
// The journal
// ---------------------------------------------------------------------

fn record(n: u64) -> Bytes {
    Bytes::from(format!("record {n}").into_bytes())
}

fn batch(first: u64, count: u64) -> Batch {
    Batch {
        first: Seq(first),
        records: (first..first + count).map(record).collect(),
    }
}

/// Puts a journal through the life of one session: begun, written to,
/// snapshotted, taken over by a new owner, and written to again.
/// `session` has to be one the journal holds nothing of.
///
/// The rules: a session nothing is stored of is new, and its first
/// record is number 1; records are stored in order and without gaps, and
/// what does not follow is refused with the number that would; a
/// snapshot replaces the records up to it, and only those; taking a
/// session over gives back what was stored, and a lease that shuts the
/// earlier owner out of everything; a refusal stores nothing.
///
/// # Errors
///
/// The rule the adapter broke.
#[expect(
    clippy::too_many_lines,
    reason = "one life of a session, told from its beginning to its end"
)]
pub async fn journal_session(journal: &dyn Journal, session: SessionId) -> Result<(), Broken> {
    let wait = |what: &'static str| format!("the journal's answer to {what}");

    // A new session.
    let first = match patiently(&wait("the first owner"), journal.acquire(session)).await? {
        Ok(recovered) => recovered,
        Err(error) => return broken(format!("the session could not be begun: {error}")),
    };
    if !first.is_new() || first.next != Seq(1) {
        return broken(format!(
            "a session nothing was stored of is not new: {first:?}"
        ));
    }
    let lease = first.lease;

    // Records in order.
    match patiently(
        &wait("an append"),
        journal.append(session, lease, batch(1, 3)),
    )
    .await?
    {
        Ok(Seq(3)) => {}
        other => return broken(format!("records 1 to 3 were not stored: {other:?}")),
    }
    // A gap, and a repeat.
    for wrong in [5, 3, 1] {
        let answer = patiently(
            &wait("an append"),
            journal.append(session, lease, batch(wrong, 1)),
        )
        .await?;
        if answer != Err(JournalError::OutOfOrder { expected: Seq(4) }) {
            return broken(format!(
                "record {wrong} after record 3 was answered with {answer:?}"
            ));
        }
    }
    match patiently(
        &wait("an append"),
        journal.append(session, lease, batch(4, 2)),
    )
    .await?
    {
        Ok(Seq(5)) => {}
        other => return broken(format!("records 4 and 5 were not stored: {other:?}")),
    }

    // A snapshot as of record 3, and one of a record that is not there.
    let state = Snapshot {
        upto: Seq(3),
        state: Bytes::from_static(b"the state as of record 3"),
    };
    if let Err(error) = patiently(
        &wait("a snapshot"),
        journal.snapshot(session, lease, state.clone()),
    )
    .await?
    {
        return broken(format!("the snapshot was not stored: {error}"));
    }
    let ahead = Snapshot {
        upto: Seq(9),
        state: Bytes::from_static(b"the state as of a record that is not stored"),
    };
    if patiently(&wait("a snapshot"), journal.snapshot(session, lease, ahead))
        .await?
        .is_ok()
    {
        return broken("a snapshot as of a record that is not stored was taken");
    }

    // A new owner gets what was stored.
    let second = match patiently(&wait("the second owner"), journal.acquire(session)).await? {
        Ok(recovered) => recovered,
        Err(error) => return broken(format!("the session could not be taken over: {error}")),
    };
    if second.lease <= lease {
        return broken("the second owner's lease does not outrank the first's");
    }
    if second.snapshot.as_ref() != Some(&state) {
        return broken(format!(
            "the snapshot did not come back: {:?}",
            second.snapshot
        ));
    }
    if second.tail != [record(4), record(5)] || second.next != Seq(6) {
        return broken(format!(
            "the records after the snapshot did not come back: {:?}, next {:?}",
            second.tail, second.next
        ));
    }

    // The first owner is shut out of everything.
    let late = patiently(
        &wait("an append"),
        journal.append(session, lease, batch(6, 1)),
    )
    .await?;
    if late != Err(JournalError::Fenced) {
        return broken(format!(
            "the earlier owner's append was answered with {late:?}"
        ));
    }
    let late = Snapshot {
        upto: Seq(5),
        state: Bytes::from_static(b"the earlier owner's state"),
    };
    let late = patiently(&wait("a snapshot"), journal.snapshot(session, lease, late)).await?;
    if late != Err(JournalError::Fenced) {
        return broken(format!(
            "the earlier owner's snapshot was answered with {late:?}"
        ));
    }

    // And the refusals stored nothing: the new owner goes on at 6.
    let lease = second.lease;
    match patiently(
        &wait("an append"),
        journal.append(session, lease, batch(6, 1)),
    )
    .await?
    {
        Ok(Seq(6)) => {}
        other => return broken(format!("record 6 was not stored: {other:?}")),
    }
    let third = match patiently(&wait("the third owner"), journal.acquire(session)).await? {
        Ok(recovered) => recovered,
        Err(error) => return broken(format!("the session could not be taken over: {error}")),
    };
    if third.snapshot.as_ref() != Some(&state)
        || third.tail != [record(4), record(5), record(6)]
        || third.next != Seq(7)
    {
        return broken(format!(
            "after refusals the session is not what was stored: {third:?}"
        ));
    }
    Ok(())
}
