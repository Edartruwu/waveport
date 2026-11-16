//! The world around the kernel: a stand-in for every adapter.
//!
//! Each stand-in does what a well-behaved adapter does, on a fixed
//! timetable, so that a scenario reads as a story and its numbers can be
//! worked out by hand. They react to the kernel's commands and put the
//! events that follow on a queue, each on the stream of the adapter that
//! would deliver it.

use std::collections::VecDeque;

use waveport_domain::command::{Command, LlmCmd, PlayoutCmd, SenseCmd, ToolCmd, TtsCmd};
use waveport_domain::event::{Event, Finish, OpEvent, OverlapVerdict, TimerId, ToolEvent, VadEdge};
use waveport_domain::ids::{AgentId, GenId, OpTag, ParticipantId, Prob, ToolCallId};
use waveport_domain::text::TextArena;
use waveport_domain::time::{MediaDur, MediaSpan, MediaTime, MonoTime};

use crate::script::Script;
use crate::setup::Names;

/// Virtual time, in milliseconds since the session began.
pub(crate) type Ms = u64;

/// How long one word takes to say, for a person and for the agent.
pub(crate) const WORD_MS: Ms = 300;
/// How long after a person stops the final transcript arrives.
const STT_FINAL_MS: Ms = 200;
/// How long after the first half of an utterance its interim transcript
/// arrives.
const STT_INTERIM_MS: Ms = 100;
/// How long an end-of-turn estimate takes.
const EOU_MS: Ms = 60;
/// How long the verdict on overlapping speech takes.
const CLASSIFY_MS: Ms = 150;
/// How long a language model takes before its first word, unless its
/// script says otherwise.
const LLM_FIRST_MS: Ms = 200;
/// The gap between the pieces a language model writes.
const LLM_PIECE_MS: Ms = 20;
/// How long speech synthesis takes to return the audio of a piece of text.
const TTS_MS: Ms = 100;
/// How long after the last text synthesis says that it is done.
const TTS_LAST_MS: Ms = 10;
/// How long a tool takes, and how long a result the scenario supplies is
/// on its way.
const TOOL_MS: Ms = 300;
/// How long the journal takes to make a fact durable.
const JOURNAL_MS: Ms = 10;
/// How often playback reports its position when nothing else prompts it.
const TICK_MS: Ms = 100;
/// What a tool returns when the scenario does not say.
const DEFAULT_RESULT: &str = r#"{"ok":true}"#;

/// The timers a scenario can lose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timer {
    /// The one that ends a closing session's time to finish.
    Drain,
    /// The one that ends its time to stop.
    Grace,
}

impl Timer {
    fn is(self, id: TimerId) -> bool {
        matches!(
            (self, id),
            (Self::Drain, TimerId::Drain) | (Self::Grace, TimerId::Grace)
        )
    }
}

/// Where an event comes from. Events of one stream arrive in the order they
/// were sent; events of different streams can arrive in any order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stream {
    /// Voice activity on one person's track.
    Voice(ParticipantId),
    /// Transcripts of one person's speech.
    Transcriber(ParticipantId),
    /// End-of-turn estimates.
    Estimator,
    /// Verdicts on overlapping speech.
    Classifier,
    /// One response's language model.
    Llm(GenId),
    /// One response's speech synthesis.
    Tts(GenId),
    /// One tool call.
    Tool(ToolCallId),
    /// The journal.
    Journal,
    /// The runtime's timers. Not an adapter: they keep working when the
    /// adapters go silent.
    Timer,
    /// The simulation's own bookkeeping.
    Local,
}

impl Stream {
    /// Whether [`Sim::race`](crate::Sim::race) delivers this stream's
    /// events: those from perception, from tools and from the journal.
    fn races(self) -> bool {
        matches!(
            self,
            Self::Voice(_)
                | Self::Transcriber(_)
                | Self::Estimator
                | Self::Classifier
                | Self::Tool(_)
                | Self::Journal
        )
    }
}

/// Something that is going to happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Arrival {
    /// The kernel is given this event.
    Event(Event),
    /// Playback reports how far it has got.
    Tick,
    /// Nothing: the simulation only wants to stop the clock here.
    Wake,
}

/// An arrival and when it is due.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pending {
    pub(crate) at: Ms,
    order: u64,
    stream: Stream,
    pub(crate) arrival: Arrival,
}

/// Everything that is going to happen, in no particular order. Sessions in
/// scenarios are small, so every operation is a scan.
#[derive(Debug, Default)]
struct Queue {
    items: Vec<Pending>,
    pushed: u64,
}

impl Queue {
    fn push(&mut self, at: Ms, stream: Stream, arrival: Arrival) {
        self.pushed += 1;
        self.items.push(Pending {
            at,
            order: self.pushed,
            stream,
            arrival,
        });
    }

    /// Removes and returns the earliest arrival due at or before `until`.
    /// Arrivals due at the same time come out in the order they went in.
    fn pop_due(&mut self, until: Ms) -> Option<Pending> {
        let index = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, pending)| pending.at <= until)
            .min_by_key(|(_, pending)| (pending.at, pending.order))
            .map(|(index, _)| index)?;
        Some(self.items.remove(index))
    }

    /// Removes and returns the next arrival of one of the streams that
    /// race. `choose` is given the number of such streams and picks one.
    fn pop_racing(&mut self, choose: impl FnOnce(usize) -> usize) -> Option<Pending> {
        let mut heads: Vec<&Pending> = Vec::new();
        for pending in self.items.iter().filter(|pending| pending.stream.races()) {
            match heads.iter_mut().find(|head| head.stream == pending.stream) {
                Some(head) if (pending.at, pending.order) < (head.at, head.order) => {
                    *head = pending;
                }
                Some(_) => {}
                None => heads.push(pending),
            }
        }
        if heads.is_empty() {
            return None;
        }
        heads.sort_by_key(|head| (head.at, head.order));
        let order = heads.get(choose(heads.len()))?.order;
        let index = self
            .items
            .iter()
            .position(|pending| pending.order == order)?;
        Some(self.items.remove(index))
    }

    /// Removes and returns everything on `stream`, in order.
    fn take_stream(&mut self, stream: Stream) -> Vec<Pending> {
        let mut taken = Vec::new();
        let mut kept = Vec::with_capacity(self.items.len());
        for pending in self.items.drain(..) {
            if pending.stream == stream {
                taken.push(pending);
            } else {
                kept.push(pending);
            }
        }
        self.items = kept;
        taken.sort_by_key(|pending| (pending.at, pending.order));
        taken
    }

    fn drop_where(&mut self, mut gone: impl FnMut(&Pending) -> bool) {
        self.items.retain(|pending| !gone(pending));
    }
}

/// How an utterance of a person is meant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Manner {
    /// Words meant to be answered.
    Speech,
    /// A listener's noise of agreement.
    Murmur,
}

/// Something a person said.
#[derive(Clone, Copy, Debug)]
struct Utterance {
    who: ParticipantId,
    manner: Manner,
    end: Ms,
}

/// One word of an agent's speech.
#[derive(Clone, Copy, Debug)]
struct Word {
    /// Where the word ends in the text sent to synthesis, in bytes.
    text_end: usize,
    /// Where the word ends in the response's audio.
    audio_end: Ms,
    /// When synthesis delivers the word's audio.
    ready: Ms,
}

/// One response's speech: what synthesis has produced and how much of it
/// the listener has heard. This is the ground truth the kernel's record of
/// what was said is checked against.
#[derive(Clone, Debug)]
pub(crate) struct Voice {
    tag: OpTag,
    text: String,
    words: Vec<Word>,
    /// When the audio synthesised last is delivered.
    last_ready: Ms,
    /// When synthesis delivers its last audio, once it has been told that
    /// no more text follows.
    finished: Option<Ms>,
    /// When playback was allowed to start.
    started: Option<Ms>,
    /// When playback stops for good.
    stopped: Option<Ms>,
    /// Audio played, as of `as_of`.
    played: Ms,
    as_of: Ms,
    /// The position and the "all played" flag last reported to the kernel.
    reported: Option<(Ms, bool)>,
}

impl Voice {
    fn new(tag: OpTag, now: Ms) -> Self {
        Self {
            tag,
            text: String::new(),
            words: Vec::new(),
            last_ready: 0,
            finished: None,
            started: None,
            stopped: None,
            played: 0,
            as_of: now,
            reported: None,
        }
    }

    /// The length of the audio synthesised so far.
    fn total(&self) -> Ms {
        self.words.last().map_or(0, |word| word.audio_end)
    }

    /// Appends text that synthesis delivers as audio at `ready`.
    fn add(&mut self, chunk: &str, ready: Ms) {
        let base = self.text.len();
        self.text.push_str(chunk);
        self.last_ready = ready;
        let mut in_word = false;
        for (offset, c) in chunk.char_indices() {
            if c.is_whitespace() {
                if in_word {
                    self.end_word(base + offset, ready);
                }
                in_word = false;
            } else {
                in_word = true;
            }
        }
        if in_word {
            self.end_word(base + chunk.len(), ready);
        }
    }

    fn end_word(&mut self, text_end: usize, ready: Ms) {
        let audio_end = self.total() + WORD_MS;
        self.words.push(Word {
            text_end,
            audio_end,
            ready,
        });
    }

    /// What synthesis tells the kernel when audio arrives.
    fn segment(&self, last: bool) -> OpEvent {
        OpEvent::TtsSegment {
            text_end: u32::try_from(self.text.len()).unwrap_or(u32::MAX),
            audio_end: audio(self.total()),
            last,
        }
    }

    /// Plays audio up to `now`: in real time from the start of playback,
    /// waiting whenever the next word's audio has not arrived.
    pub(crate) fn advance(&mut self, now: Ms) {
        if let Some(started) = self.started {
            let limit = self.stopped.map_or(now, |stop| stop.min(now));
            let mut at = self.as_of.max(started);
            for word in &self.words {
                if word.audio_end <= self.played {
                    continue;
                }
                let begin = at.max(word.ready);
                if begin >= limit {
                    break;
                }
                let play = (word.audio_end - self.played).min(limit - begin);
                self.played += play;
                at = begin + play;
                if self.played < word.audio_end {
                    break;
                }
            }
        }
        self.as_of = self.as_of.max(now);
    }

    /// When playback reaches `position`, if nothing stops it and the audio
    /// up to there is already on its way. Call [`advance`](Self::advance)
    /// first.
    fn eta(&self, now: Ms, position: Ms) -> Option<Ms> {
        let started = self.started?;
        if self.stopped.is_some() {
            return None;
        }
        if self.played >= position {
            return Some(now);
        }
        let mut at = now.max(started);
        let mut played = self.played;
        for word in &self.words {
            if word.audio_end <= played {
                continue;
            }
            at = at.max(word.ready) + (word.audio_end - played);
            played = word.audio_end;
            if played >= position {
                return Some(at);
            }
        }
        None
    }

    /// When the listener will have heard `count` whole words.
    pub(crate) fn word_eta(&self, now: Ms, count: usize) -> Option<Ms> {
        let word = self.words.get(count.checked_sub(1)?)?;
        self.eta(now, word.audio_end)
    }

    /// How many whole words the listener has heard.
    pub(crate) fn heard_words(&self) -> usize {
        self.words
            .iter()
            .take_while(|word| word.audio_end <= self.played)
            .count()
    }

    /// The text the listener has heard in full.
    pub(crate) fn heard_text(&self) -> &str {
        let end = self
            .words
            .iter()
            .take_while(|word| word.audio_end <= self.played)
            .last()
            .map_or(0, |word| word.text_end);
        self.text.get(..end).unwrap_or("")
    }

    /// Whether every word synthesis will deliver has been played.
    fn is_drained(&self, now: Ms) -> bool {
        self.finished.is_some_and(|at| at <= now) && self.played >= self.total()
    }

    /// Whether the listener will hear no more of this response.
    pub(crate) fn is_over(&self, now: Ms) -> bool {
        self.stopped.is_some_and(|stop| stop <= now) || self.is_drained(now)
    }

    /// Whether playback has more to report.
    fn is_playing(&self) -> bool {
        self.started.is_some()
            && self.stopped.is_none()
            && !matches!(self.reported, Some((_, true)))
    }

    /// The position report playback owes the kernel, if the position has
    /// changed since the last one.
    fn report(&mut self, now: Ms) -> Option<OpEvent> {
        self.advance(now);
        if !self.is_playing() {
            return None;
        }
        let state = (self.played, self.is_drained(now));
        if self.reported == Some(state) || state == (0, false) {
            return None;
        }
        self.reported = Some(state);
        let position = audio(self.played);
        Some(OpEvent::Played {
            handed: position,
            acked: position,
            heard_text: u32::try_from(self.heard_text().len()).unwrap_or(u32::MAX),
            drained: state.1,
        })
    }

    /// When playback next has something to report by itself: at the usual
    /// interval, or sooner if the response's audio ends before that.
    fn next_report(&self, now: Ms) -> Ms {
        let end = self
            .finished
            .and_then(|finished| Some(self.eta(now, self.total())?.max(finished)))
            .filter(|end| *end > now);
        end.map_or(now + TICK_MS, |end| end.min(now + TICK_MS))
    }
}

/// A language model whose output the kernel has asked not to receive for
/// now.
#[derive(Debug)]
struct Paused {
    stream: Stream,
    since: Ms,
    held: Vec<Pending>,
}

/// Every adapter of one session.
#[derive(Debug)]
pub(crate) struct World {
    pub(crate) names: Names,
    queue: Queue,
    /// No adapter says anything any more.
    silent: bool,
    /// Tools return only what the scenario supplies.
    tools_wait: bool,
    lost: Vec<Timer>,
    utterances: Vec<Utterance>,
    /// What each agent's language model will answer, by agent.
    scripts: Vec<VecDeque<Script>>,
    voices: Vec<Voice>,
    paused: Vec<Paused>,
    /// When playback next reports by itself.
    tick_at: Option<Ms>,
    /// When the simulation last asked to be woken.
    wake_at: Option<Ms>,
    /// Things that must never happen in any scenario, and did.
    faults: Vec<String>,
}

impl World {
    pub(crate) fn new(names: Names) -> Self {
        let scripts = (0..names.agent_count()).map(|_| VecDeque::new()).collect();
        Self {
            names,
            queue: Queue::default(),
            silent: false,
            tools_wait: false,
            lost: Vec::new(),
            utterances: Vec::new(),
            scripts,
            voices: Vec::new(),
            paused: Vec::new(),
            tick_at: None,
            wake_at: None,
            faults: Vec::new(),
        }
    }

    /// Records something that must never happen.
    pub(crate) fn blame(&mut self, fault: String) {
        self.faults.push(fault);
    }

    /// The first thing that must never happen and did.
    pub(crate) fn fault(&self) -> Option<&str> {
        self.faults.first().map(String::as_str)
    }

    /// Queues the next response of `agent`'s language model.
    pub(crate) fn script(&mut self, agent: AgentId, script: Script) {
        match self.scripts.get_mut(usize::from(agent.get())) {
            Some(scripts) => scripts.push_back(script),
            None => self.blame(format!(
                "the scenario scripts agent {agent}, which it did not set up"
            )),
        }
    }

    /// The responses that were scripted and never asked for.
    pub(crate) fn unused_scripts(&self) -> Vec<String> {
        self.scripts
            .iter()
            .zip(0_u8..)
            .flat_map(|(scripts, agent)| {
                scripts.iter().map(move |script| {
                    format!(
                        "{} never gave the response \"{}\"",
                        self.names.agent(AgentId(agent)),
                        script.text
                    )
                })
            })
            .collect()
    }

    /// Sends an adapter's event, unless the adapters have gone silent.
    fn send(&mut self, at: Ms, stream: Stream, event: Event) {
        if !self.silent {
            self.queue.push(at, stream, Arrival::Event(event));
        }
    }

    /// From now on no adapter says anything, including what is already on
    /// its way. Timers keep working.
    pub(crate) fn go_silent(&mut self) {
        self.silent = true;
        self.queue
            .drop_where(|pending| pending.stream != Stream::Timer);
        self.paused.clear();
    }

    pub(crate) fn is_silent(&self) -> bool {
        self.silent
    }

    /// From now on firings of `timer` never arrive.
    pub(crate) fn lose(&mut self, timer: Timer) {
        self.lost.push(timer);
    }

    pub(crate) fn loses(&self, id: TimerId) -> bool {
        self.lost.iter().any(|timer| timer.is(id))
    }

    /// From now on tools return only what the scenario supplies.
    pub(crate) fn hold_tools(&mut self) {
        self.tools_wait = true;
    }

    /// Puts a result for `call` on its way.
    pub(crate) fn tool_returns(&mut self, now: Ms, call: ToolCallId, result: &str) {
        let event = Event::Tool(
            call,
            ToolEvent::Completed {
                result: result.into(),
            },
        );
        self.send(now + TOOL_MS, Stream::Tool(call), event);
    }

    /// Stops the clock at `at` even if nothing else happens then.
    pub(crate) fn wake(&mut self, at: Ms) {
        if self.wake_at != Some(at) {
            self.wake_at = Some(at);
            self.queue.push(at, Stream::Local, Arrival::Wake);
        }
    }

    pub(crate) fn pop_due(&mut self, until: Ms) -> Option<Pending> {
        self.queue.pop_due(until)
    }

    /// Whether anything at all is still going to happen.
    pub(crate) fn has_more(&self) -> bool {
        !self.queue.items.is_empty()
    }

    pub(crate) fn pop_racing(&mut self, choose: impl FnOnce(usize) -> usize) -> Option<Pending> {
        self.queue.pop_racing(choose)
    }

    /// How many times `who` has spoken.
    pub(crate) fn utterances_by(&self, who: ParticipantId) -> usize {
        self.utterances
            .iter()
            .filter(|utterance| utterance.who == who)
            .count()
    }

    fn latest_utterance(&self, who: ParticipantId) -> Option<&Utterance> {
        self.utterances
            .iter()
            .rev()
            .find(|utterance| utterance.who == who)
    }

    /// `who` starts to say `text`. Returns the event that reports the start
    /// of their speech; the rest follows on the timetable.
    pub(crate) fn utter(
        &mut self,
        now: Ms,
        who: ParticipantId,
        text: &str,
        manner: Manner,
    ) -> Option<Event> {
        let words = text.split_whitespace().collect::<Vec<_>>();
        if words.is_empty() {
            self.blame(format!(
                "the scenario has {} say nothing",
                self.names.person(who)
            ));
            return None;
        }
        if self
            .latest_utterance(who)
            .is_some_and(|utterance| utterance.end > now)
        {
            self.blame(format!(
                "the scenario has {} say \"{text}\" while still saying something else",
                self.names.person(who)
            ));
            return None;
        }
        let end = now + WORD_MS * count(words.len());
        self.utterances.push(Utterance { who, manner, end });
        if self.silent {
            return None;
        }

        let (start_at, end_at) = (track(now), track(end));
        let stopped = Event::Speech {
            who,
            edge: VadEdge::End,
            at: end_at,
        };
        self.send(end, Stream::Voice(who), stopped);
        let half = words.len() / 2;
        if half > 0 {
            let heard = now + WORD_MS * count(half);
            let interim = Event::Transcript {
                who,
                span: MediaSpan::new(start_at, track(heard)),
                text: words.get(..half).unwrap_or_default().join(" ").into(),
                is_final: false,
            };
            self.send(heard + STT_INTERIM_MS, Stream::Transcriber(who), interim);
        }
        let transcript = Event::Transcript {
            who,
            span: MediaSpan::new(start_at, end_at),
            text: words.join(" ").into(),
            is_final: true,
        };
        self.send(end + STT_FINAL_MS, Stream::Transcriber(who), transcript);
        Some(Event::Speech {
            who,
            edge: VadEdge::Start,
            at: start_at,
        })
    }

    /// Has the adapter a command is for react to it.
    pub(crate) fn on_command(&mut self, now: Ms, command: &Command, text: &TextArena) {
        match *command {
            Command::Llm(tag, command) => self.llm(now, tag, command),
            Command::Tts(tag, command) => self.tts(now, tag, command, text),
            Command::Playout(tag, command) => self.playout(now, tag, command),
            Command::Sense(command) => self.sense(now, command),
            Command::Tool(call, ToolCmd::Dispatch { .. }) if !self.tools_wait => {
                self.tool_returns(now, call, DEFAULT_RESULT);
            }
            Command::SetTimer(id, at) => self.set_timer(now, id, at),
            Command::AwaitDurable(seq) => {
                self.send(now + JOURNAL_MS, Stream::Journal, Event::Durable(seq));
            }
            // The transcriber here decides by itself when a person has
            // finished, a tool keeps running when it is cancelled, and
            // nobody is listening to client events.
            Command::Tool(..) | Command::Stt(_) | Command::Emit(_) => {}
        }
    }

    fn llm(&mut self, now: Ms, tag: OpTag, command: LlmCmd) {
        let stream = Stream::Llm(tag.generation);
        match command {
            LlmCmd::Start { agent, cause, .. } => {
                let script = self
                    .scripts
                    .get_mut(usize::from(agent.get()))
                    .and_then(VecDeque::pop_front);
                let Some(script) = script else {
                    self.blame(format!(
                        "{} started a response that the scenario did not script, for {cause:?}",
                        self.names.agent(agent)
                    ));
                    return;
                };
                let think = script.think.map_or(LLM_FIRST_MS, |think| {
                    u64::try_from(think.as_millis()).unwrap_or(u64::MAX)
                });
                let mut at = now.saturating_add(think);
                for piece in script.text.split_inclusive(' ') {
                    let delta = OpEvent::LlmDelta { text: piece.into() };
                    self.send(at, stream, Event::Op(tag, delta));
                    at += LLM_PIECE_MS;
                }
                for (tool, args) in &script.calls {
                    let Some(tool) = self.names.tool_id(tool) else {
                        self.blame(format!(
                            "the scenario scripts a call to `{tool}`, which it did not set up"
                        ));
                        continue;
                    };
                    let proposed = OpEvent::ToolProposed {
                        tool,
                        args: args.as_str().into(),
                    };
                    self.send(at, stream, Event::Op(tag, proposed));
                    at += LLM_PIECE_MS;
                }
                let finish = if script.calls.is_empty() {
                    Finish::Stop
                } else {
                    Finish::ToolCalls
                };
                self.send(at, stream, Event::Op(tag, OpEvent::LlmDone { finish }));
            }
            LlmCmd::Pause => {
                if self.paused.iter().all(|paused| paused.stream != stream) {
                    let held = self.queue.take_stream(stream);
                    self.paused.push(Paused {
                        stream,
                        since: now,
                        held,
                    });
                }
            }
            LlmCmd::Resume => {
                if let Some(index) = self
                    .paused
                    .iter()
                    .position(|paused| paused.stream == stream)
                {
                    let paused = self.paused.remove(index);
                    for pending in paused.held {
                        let at = now + pending.at.saturating_sub(paused.since);
                        self.queue.push(at, stream, pending.arrival);
                    }
                }
            }
            LlmCmd::Cancel => {
                self.queue.drop_where(|pending| pending.stream == stream);
                self.paused.retain(|paused| paused.stream != stream);
            }
        }
    }

    fn tts(&mut self, now: Ms, tag: OpTag, command: TtsCmd, text: &TextArena) {
        if self.silent {
            return;
        }
        let stream = Stream::Tts(tag.generation);
        let Some(voice) = self.voice_mut(tag, now) else {
            return;
        };
        match command {
            TtsCmd::Speak { text: chunk } => {
                let ready = (now + TTS_MS).max(voice.last_ready);
                voice.add(text.get(chunk), ready);
                let segment = voice.segment(false);
                self.send(ready, stream, Event::Op(tag, segment));
            }
            TtsCmd::Finish => {
                let at = (now + TTS_LAST_MS).max(voice.last_ready);
                voice.last_ready = at;
                voice.finished = Some(at);
                let segment = voice.segment(true);
                self.send(at, stream, Event::Op(tag, segment));
            }
            TtsCmd::Cancel => {
                // Audio that has not been delivered never is.
                voice.words.retain(|word| word.ready <= now);
                self.queue.drop_where(|pending| pending.stream == stream);
            }
        }
    }

    fn playout(&mut self, now: Ms, tag: OpTag, command: PlayoutCmd) {
        let Some(voice) = self.voice_mut(tag, now) else {
            return;
        };
        voice.advance(now);
        match command {
            PlayoutCmd::Start => {
                voice.started.get_or_insert(now);
            }
            PlayoutCmd::Stop { fade_ms } => {
                voice.stopped.get_or_insert(now + Ms::from(fade_ms));
            }
            // A lowered volume changes nothing about what is heard.
            PlayoutCmd::Duck | PlayoutCmd::Unduck => {}
        }
    }

    fn sense(&mut self, now: Ms, command: SenseCmd) {
        match command {
            SenseCmd::EstimateEou { turn, who } => {
                // The estimator is right: the speaker has finished exactly
                // when the end of everything the scenario gave them to say
                // has been reported. It goes by what was reported, not by
                // the clock, because a race delivers that report early.
                let speaking = self
                    .queue
                    .items
                    .iter()
                    .any(|pending| pending.stream == Stream::Voice(who));
                let p = Prob::from_basis_points(if speaking { 1_000 } else { 9_500 });
                self.send(now + EOU_MS, Stream::Estimator, Event::Eou { turn, p });
            }
            SenseCmd::ClassifyOverlap { turn, who } => {
                // So is the classifier: it knows how the words were meant.
                let verdict = match self.latest_utterance(who).map(|utterance| utterance.manner) {
                    Some(Manner::Speech) => OverlapVerdict::BargeIn,
                    Some(Manner::Murmur) => OverlapVerdict::Backchannel,
                    None => OverlapVerdict::Noise,
                };
                let event = Event::Overlap { turn, verdict };
                self.send(now + CLASSIFY_MS, Stream::Classifier, event);
            }
        }
    }

    fn set_timer(&mut self, now: Ms, id: TimerId, at: Option<MonoTime>) {
        self.queue
            .drop_where(|pending| pending.arrival == Arrival::Event(Event::Timer(id)));
        if let Some(at) = at {
            // A timer never fires early, and never in the past.
            let at = at.as_nanos().div_ceil(1_000_000).max(now);
            self.queue
                .push(at, Stream::Timer, Arrival::Event(Event::Timer(id)));
        }
    }

    /// The speech carrying `tag`, begun now if there is none yet.
    fn voice_mut(&mut self, tag: OpTag, now: Ms) -> Option<&mut Voice> {
        if self.voices.iter().all(|voice| voice.tag != tag) {
            self.voices.push(Voice::new(tag, now));
        }
        self.voices.iter_mut().find(|voice| voice.tag == tag)
    }

    /// The speech of `generation`, if synthesis or playback has been asked
    /// for any.
    pub(crate) fn voice_of(&mut self, generation: GenId) -> Option<&mut Voice> {
        self.voices
            .iter_mut()
            .find(|voice| voice.tag.generation == generation)
    }

    /// The position reports playback owes the kernel. The runtime delivers
    /// these ahead of any other event.
    pub(crate) fn reports(&mut self, now: Ms) -> Vec<(OpTag, OpEvent)> {
        if self.silent {
            return Vec::new();
        }
        self.voices
            .iter_mut()
            .filter_map(|voice| Some((voice.tag, voice.report(now)?)))
            .collect()
    }

    /// Makes sure playback reports again by itself while anything plays.
    pub(crate) fn schedule_tick(&mut self, now: Ms) {
        if self.silent {
            return;
        }
        for voice in &mut self.voices {
            voice.advance(now);
        }
        let next = self
            .voices
            .iter()
            .filter(|voice| voice.is_playing())
            .map(|voice| voice.next_report(now))
            .min();
        if let Some(next) = next
            && self.tick_at.is_none_or(|at| at <= now || next < at)
        {
            self.tick_at = Some(next);
            self.queue.push(next, Stream::Local, Arrival::Tick);
        }
    }
}

/// A count as a number of milliseconds or words.
fn count(items: usize) -> Ms {
    Ms::try_from(items).unwrap_or(Ms::MAX)
}

/// A length of audio.
fn audio(millis: Ms) -> MediaDur {
    MediaDur::from_millis(u32::try_from(millis).unwrap_or(u32::MAX))
}

/// A position on a person's track. Every track starts with the session.
fn track(millis: Ms) -> MediaTime {
    MediaTime::from_millis(millis)
}

#[cfg(test)]
mod tests {
    use waveport_domain::command::{Command, LlmCause, LlmCmd, PlayoutCmd, SenseCmd, TtsCmd};
    use waveport_domain::event::{Event, Finish, OpEvent, OverlapVerdict, TimerId, VadEdge};
    use waveport_domain::ids::{
        AgentId, ContextVersion, Epoch, GenId, OpTag, ParticipantId, Prob, Seq, ToolCallId, ToolId,
        TurnId,
    };
    use waveport_domain::text::TextArena;
    use waveport_domain::time::{MediaDur, MonoTime};

    use super::{Arrival, Manner, Ms, Timer, World};
    use crate::script::say;
    use crate::setup::Setup;
    use waveport_domain::config::Effect;

    const TAG: OpTag = OpTag {
        generation: GenId(1),
        epoch: Epoch(0),
    };
    const CALLER: ParticipantId = ParticipantId(1);

    fn world() -> World {
        let setup = Setup::new().tool("lookup", Effect::Pure);
        World::new(setup.build().unwrap().2)
    }

    /// Everything due up to `until`, as `(time, event)`.
    fn events(world: &mut World, until: Ms) -> Vec<(Ms, Event)> {
        let mut events = Vec::new();
        while let Some(pending) = world.pop_due(until) {
            if let Arrival::Event(event) = pending.arrival {
                events.push((pending.at, event));
            }
        }
        events
    }

    fn speak(world: &mut World, now: Ms, text: &str) {
        let mut arena = TextArena::default();
        let text = arena.push(text);
        world.on_command(now, &Command::Tts(TAG, TtsCmd::Speak { text }), &arena);
    }

    fn command(world: &mut World, now: Ms, command: Command) {
        world.on_command(now, &command, &TextArena::default());
    }

    #[test]
    fn a_person_speaking_is_reported_on_the_timetable() {
        let mut world = world();
        let started = world.utter(1_000, CALLER, "where is my order", Manner::Speech);
        assert!(matches!(
            started,
            Some(Event::Speech {
                who: CALLER,
                edge: VadEdge::Start,
                ..
            })
        ));
        let events = events(&mut world, 10_000);
        let times = events.iter().map(|(at, _)| *at).collect::<Vec<_>>();
        // Two of four words heard at 1.6 s, the end at 2.2 s.
        assert_eq!(times, [1_700, 2_200, 2_400]);
        assert!(matches!(
            &events[0].1,
            Event::Transcript { text, is_final: false, .. } if text.as_str() == "where is"
        ));
        assert!(matches!(
            events[1].1,
            Event::Speech {
                edge: VadEdge::End,
                ..
            }
        ));
        assert!(matches!(
            &events[2].1,
            Event::Transcript { text, span, is_final: true, .. }
                if text.as_str() == "where is my order" && span.len() == MediaDur::from_millis(1_200)
        ));
        assert_eq!(world.utterances_by(CALLER), 1);
        assert_eq!(world.fault(), None);
    }

    #[test]
    fn nobody_says_two_things_at_once() {
        let mut world = world();
        assert!(world.utter(0, CALLER, "one two", Manner::Speech).is_some());
        assert!(world.utter(300, CALLER, "three", Manner::Speech).is_none());
        assert!(world.fault().unwrap().contains("while still saying"));
    }

    #[test]
    fn the_turn_taking_models_know_what_the_scenario_knows() {
        let mut world = world();
        let turn = TurnId(1);
        let who = CALLER;
        let unfinished = Prob::from_basis_points(1_000);
        let finished = Prob::from_basis_points(9_500);
        let overlap = |verdict| Event::Overlap { turn, verdict };

        // Asked while the murmur is still being made.
        let _ = world.utter(0, who, "mm-hm", Manner::Murmur);
        let classify = Command::Sense(SenseCmd::ClassifyOverlap { turn, who });
        let estimate = Command::Sense(SenseCmd::EstimateEou { turn, who });
        command(&mut world, 100, classify);
        command(&mut world, 100, estimate);
        let answers = events(&mut world, 300);
        assert_eq!(
            answers.get(..2),
            Some(
                &[
                    (
                        160,
                        Event::Eou {
                            turn,
                            p: unfinished
                        }
                    ),
                    (250, overlap(OverlapVerdict::Backchannel)),
                ][..]
            )
        );
        assert!(matches!(
            answers.get(2),
            Some((
                300,
                Event::Speech {
                    edge: VadEdge::End,
                    ..
                }
            ))
        ));

        // Asked again once its end has been reported.
        command(&mut world, 300, estimate);
        assert_eq!(
            events(&mut world, 400),
            [(360, Event::Eou { turn, p: finished })]
        );

        // Words that are meant are a barge-in, and someone who has said
        // nothing made a noise.
        let _ = world.utter(1_000, who, "stop", Manner::Speech);
        command(&mut world, 1_000, classify);
        let stranger = SenseCmd::ClassifyOverlap {
            turn,
            who: ParticipantId(9),
        };
        command(&mut world, 1_000, Command::Sense(stranger));
        let verdicts = events(&mut world, 1_150)
            .into_iter()
            .filter(|(_, event)| matches!(event, Event::Overlap { .. }))
            .collect::<Vec<_>>();
        assert_eq!(
            verdicts,
            [
                (1_150, overlap(OverlapVerdict::BargeIn)),
                (1_150, overlap(OverlapVerdict::Noise)),
            ]
        );
    }

    #[test]
    fn a_language_model_writes_its_script_and_nothing_else() {
        let mut world = world();
        world.script(AgentId(0), say("Let me look.").call("lookup", "{}"));
        let start = LlmCmd::Start {
            agent: AgentId(0),
            context: ContextVersion(1),
            cause: LlmCause::Reply { turn: TurnId(1) },
        };
        command(&mut world, 0, Command::Llm(TAG, start));
        let written = events(&mut world, 10_000);
        let expected = [
            (
                200,
                OpEvent::LlmDelta {
                    text: "Let ".into(),
                },
            ),
            (220, OpEvent::LlmDelta { text: "me ".into() }),
            (
                240,
                OpEvent::LlmDelta {
                    text: "look.".into(),
                },
            ),
            (
                260,
                OpEvent::ToolProposed {
                    tool: ToolId(0),
                    args: "{}".into(),
                },
            ),
            (
                280,
                OpEvent::LlmDone {
                    finish: Finish::ToolCalls,
                },
            ),
        ]
        .map(|(at, op)| (at, Event::Op(TAG, op)));
        assert_eq!(written, expected);
        assert_eq!(world.unused_scripts(), Vec::<String>::new());

        // A second response was not scripted.
        command(&mut world, 500, Command::Llm(TAG, start));
        assert!(world.fault().unwrap().contains("did not script"));
    }

    #[test]
    fn a_paused_language_model_resumes_where_it_stopped() {
        let mut world = world();
        world.script(AgentId(0), say("one two three"));
        let start = LlmCmd::Start {
            agent: AgentId(0),
            context: ContextVersion(1),
            cause: LlmCause::Reply { turn: TurnId(1) },
        };
        command(&mut world, 0, Command::Llm(TAG, start));
        assert_eq!(events(&mut world, 200).len(), 1);
        command(&mut world, 210, Command::Llm(TAG, LlmCmd::Pause));
        assert_eq!(events(&mut world, 1_000), []);
        command(&mut world, 1_000, Command::Llm(TAG, LlmCmd::Resume));
        let times = events(&mut world, 5_000)
            .iter()
            .map(|(at, _)| *at)
            .collect::<Vec<_>>();
        assert_eq!(times, [1_010, 1_030, 1_050]);

        world.script(AgentId(0), say("never written"));
        command(&mut world, 2_000, Command::Llm(TAG, start));
        command(&mut world, 2_000, Command::Llm(TAG, LlmCmd::Cancel));
        assert_eq!(events(&mut world, 9_000), []);
    }

    #[test]
    fn speech_is_heard_a_word_at_a_time() {
        let mut world = world();
        speak(&mut world, 0, "Your order shipped ");
        command(&mut world, 0, Command::Playout(TAG, PlayoutCmd::Start));
        let segment = events(&mut world, 100);
        assert_eq!(
            segment,
            [(
                100,
                Event::Op(
                    TAG,
                    OpEvent::TtsSegment {
                        text_end: 19,
                        audio_end: MediaDur::from_millis(900),
                        last: false,
                    }
                )
            )]
        );

        // Audio arrives at 100 ms, so the second word ends at 700 ms.
        let voice = world.voice_of(GenId(1)).unwrap();
        voice.advance(50);
        assert_eq!(voice.heard_words(), 0);
        assert_eq!(voice.word_eta(50, 2), Some(700));
        assert_eq!(voice.word_eta(50, 4), None);
        voice.advance(699);
        assert_eq!(voice.heard_text(), "Your");
        voice.advance(700);
        assert_eq!(voice.heard_text(), "Your order");
        assert!(!voice.is_over(700));

        // More text arrives late: playback waits for it.
        speak(&mut world, 2_000, "on Monday.");
        command(&mut world, 2_000, Command::Tts(TAG, TtsCmd::Finish));
        let voice = world.voice_of(GenId(1)).unwrap();
        voice.advance(2_000);
        assert_eq!(voice.heard_text(), "Your order shipped");
        assert_eq!(voice.word_eta(2_000, 5), Some(2_700));
        voice.advance(2_700);
        assert_eq!(voice.heard_text(), "Your order shipped on Monday.");
        assert!(voice.is_over(2_700));
    }

    #[test]
    fn playback_reports_changes_and_then_its_end() {
        let mut world = world();
        speak(&mut world, 0, "Hello there.");
        command(&mut world, 0, Command::Tts(TAG, TtsCmd::Finish));
        command(&mut world, 0, Command::Playout(TAG, PlayoutCmd::Start));
        assert_eq!(world.reports(100), [], "nothing has been played yet");

        let played = |millis, heard_text, drained| OpEvent::Played {
            handed: MediaDur::from_millis(millis),
            acked: MediaDur::from_millis(millis),
            heard_text,
            drained,
        };
        assert_eq!(world.reports(400), [(TAG, played(300, 5, false))]);
        assert_eq!(world.reports(400), [], "nothing has changed");
        assert_eq!(world.reports(900), [(TAG, played(600, 12, true))]);
        assert_eq!(world.reports(5_000), [], "the end is reported once");
    }

    #[test]
    fn playback_ticks_until_the_audio_ends() {
        let mut world = world();
        speak(&mut world, 0, "Hello there.");
        command(&mut world, 0, Command::Tts(TAG, TtsCmd::Finish));
        command(&mut world, 0, Command::Playout(TAG, PlayoutCmd::Start));
        let mut ticks = Vec::new();
        let mut now = 0;
        loop {
            world.schedule_tick(now);
            let Some(pending) = world.pop_due(10_000) else {
                break;
            };
            now = pending.at;
            if pending.arrival == Arrival::Tick {
                ticks.push(now);
                let _ = world.reports(now);
            }
        }
        // Every 100 ms, and exactly when the last word ends at 700 ms.
        assert_eq!(ticks, [100, 200, 300, 400, 500, 600, 700]);
    }

    #[test]
    fn a_stopped_voice_is_heard_no_further() {
        let mut world = world();
        speak(&mut world, 0, "one two three four");
        command(&mut world, 0, Command::Playout(TAG, PlayoutCmd::Start));
        command(
            &mut world,
            750,
            Command::Playout(TAG, PlayoutCmd::Stop { fade_ms: 8 }),
        );
        command(&mut world, 750, Command::Tts(TAG, TtsCmd::Cancel));
        let voice = world.voice_of(GenId(1)).unwrap();
        voice.advance(5_000);
        assert_eq!(voice.heard_text(), "one two");
        assert!(voice.is_over(5_000));
        assert_eq!(voice.word_eta(5_000, 3), None);
        assert_eq!(world.reports(5_000), []);
    }

    #[test]
    fn timers_are_replaced_cancelled_and_lost() {
        let mut world = world();
        let at = |millis| Some(MonoTime::from_millis(millis));
        command(&mut world, 0, Command::SetTimer(TimerId::Drain, at(500)));
        command(&mut world, 0, Command::SetTimer(TimerId::Drain, at(800)));
        command(&mut world, 0, Command::SetTimer(TimerId::Grace, at(900)));
        command(
            &mut world,
            0,
            Command::SetTimer(TimerId::Checkpoint, at(100)),
        );
        command(&mut world, 0, Command::SetTimer(TimerId::Checkpoint, None));
        // One nanosecond past 950 ms is not before 951 ms.
        let late = Some(MonoTime::from_nanos(950_000_001));
        command(&mut world, 0, Command::SetTimer(TimerId::StageAck, late));
        assert_eq!(
            events(&mut world, 10_000),
            [
                (800, Event::Timer(TimerId::Drain)),
                (900, Event::Timer(TimerId::Grace)),
                (951, Event::Timer(TimerId::StageAck)),
            ]
        );
        world.lose(Timer::Drain);
        assert!(world.loses(TimerId::Drain));
        assert!(!world.loses(TimerId::Grace));
    }

    #[test]
    fn silent_adapters_say_nothing_and_timers_still_fire() {
        let mut world = world();
        let _ = world.utter(0, CALLER, "hello there", Manner::Speech);
        command(&mut world, 0, Command::AwaitDurable(Seq(1)));
        command(
            &mut world,
            0,
            Command::SetTimer(TimerId::Grace, Some(MonoTime::from_millis(50))),
        );
        world.go_silent();
        assert!(world.is_silent());
        world.tool_returns(0, ToolCallId(1), "{}");
        speak(&mut world, 0, "anyone");
        assert_eq!(
            events(&mut world, 10_000),
            [(50, Event::Timer(TimerId::Grace))]
        );
    }

    #[test]
    fn racing_takes_the_head_of_any_perception_stream() {
        let mut world = world();
        let _ = world.utter(0, CALLER, "one two", Manner::Speech);
        world.tool_returns(0, ToolCallId(7), "{}");
        command(
            &mut world,
            0,
            Command::SetTimer(TimerId::Grace, Some(MonoTime::from_millis(1))),
        );
        // Three streams race: the tool (due at 300 ms), the transcriber
        // (interim at 400 ms, final at 800 ms) and the voice (600 ms). The
        // timer does not. Taking the last stream each time:
        let mut ways = Vec::new();
        let mut order = Vec::new();
        while let Some(pending) = world.pop_racing(|of| {
            ways.push(of);
            of - 1
        }) {
            order.push(pending.at);
        }
        assert_eq!(ways, [3, 2, 2, 1]);
        // The final transcript cannot overtake the interim one.
        assert_eq!(order, [600, 400, 800, 300]);
        assert_eq!(events(&mut world, 10).len(), 1, "the timer is still due");
    }
}
