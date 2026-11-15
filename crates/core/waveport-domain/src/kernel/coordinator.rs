//! One step of a session: routing an event to the machine it concerns,
//! and everything that crosses from one machine to another.
//!
//! Routing only delegates. What one machine does to another passes through
//! a few functions, each the only place its effect can happen:
//!
//! - [`Step::interrupt`] ends the responses that hold the floor, and is the
//!   only place the epoch moves.
//! - [`Step::utterance_ended`] is the only place an agent's speech enters
//!   the conversation.
//! - [`Step::turn_ended`] is the only place a user turn is committed or
//!   dismissed.
//! - [`Step::call_finished`] is the only place a tool call ends and its
//!   result enters the conversation.
//! - [`Step::settle`] runs after every event and applies, in a fixed
//!   order, the rules that start things: end the turns that can end, move
//!   the tool calls on, start the response that is owed, start playback,
//!   move the session through its life.

use core::time::Duration;

use super::State;
use super::floor::{OpenTurn, Overlap};
use super::lifecycle::Closing;
use super::response::{Generation, Playback, Source, Synthesis};
use super::tools::{self, Call};
use super::view::{CallState, GenerationRole};
use crate::command::{
    Activity, ClientEvent, Command, LlmCause, LlmCmd, PlayoutCmd, SenseCmd, SttCmd, ToolCmd, TtsCmd,
};
use crate::config::{Effect, Executor, KernelConfig};
use crate::event::{
    CloseReason, ControlInput, Event, HandoffContext, OpEvent, OverlapVerdict, Part, TimerId,
    ToolEvent, VadEdge,
};
use crate::fact::{Dismissal, EpochCause, Fact, Lifecycle, NoticeKind, Stamped, ToolOutcome};
use crate::ids::{AgentId, GenId, OpTag, ParticipantId, Prob, Seq, ToolCallId, ToolId, TurnId};
use crate::out::Out;
use crate::policy::{
    AddresseeInput, AddresseePolicy, GateInput, InterruptionPolicy, OverlapDecision, OverlapInput,
    Policies, TextChunker, ToolGate, TurnDecision, TurnInput, TurnPolicy,
};
use crate::time::{MediaDur, MediaSpan, MediaTime, MonoTime};

/// What became of a tool call when the session looked at it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Moved {
    /// Nothing: it waits for something.
    Waits,
    /// It is a step further and can be looked at again.
    On,
    /// It ended, and is no longer in the table.
    Ended,
}

/// Everything one step works with.
pub(super) struct Step<'a, P: Policies> {
    pub(super) now: MonoTime,
    pub(super) config: &'a KernelConfig,
    pub(super) policies: &'a P,
    pub(super) state: &'a mut State,
    pub(super) out: &'a mut Out,
}

/// Whether `text` holds anything but white space.
fn says_something(text: &str) -> bool {
    text.chars().any(|character| !character.is_whitespace())
}

impl<P: Policies> Step<'_, P> {
    /// Takes one event.
    #[inline]
    pub(super) fn run(&mut self, event: &Event) {
        match event {
            Event::Speech { who, edge, at } => match edge {
                VadEdge::Start => self.speech_started(*who, *at),
                VadEdge::End => self.speech_stopped(*who, *at),
            },
            Event::Transcript {
                who,
                span,
                text,
                is_final,
            } => {
                // Interim transcripts change nothing: the words of a turn
                // are the final ones.
                if *is_final {
                    self.transcribed(*who, *span, text.as_str());
                }
            }
            Event::Eou { turn, p } => self.estimated(*turn, *p),
            Event::Overlap { turn, verdict } => self.judged(*turn, *verdict),
            Event::TextIn { who, text } => self.typed(*who, text.as_str()),
            Event::Op(tag, op) => self.op(*tag, op),
            Event::Timer(id) => self.timer_fired(*id),
            Event::Control(input) => self.control(input),
            Event::Tool(call, news) => self.tool(*call, news),
            Event::Durable(seq) => self.stored(*seq),
        }
        self.settle();
    }

    // ---- Emitting ----------------------------------------------------------

    fn cmd(&mut self, command: Command) {
        self.out.cmds.push(command);
    }

    /// Journals a fact and makes it true. This is the only way the part of
    /// the state that outlives a crash changes.
    fn fact(&mut self, fact: Fact) {
        let _ = self.journal(fact);
    }

    /// Journals a fact, makes it true, and returns its place in the
    /// journal.
    fn journal(&mut self, fact: Fact) -> Seq {
        let seq = self.state.next_seq;
        self.state.next_seq = seq.next();
        self.state.apply(&fact, &self.out.text);
        self.out.facts.push(Stamped { seq, fact });
        seq
    }

    fn set_timer(&mut self, id: TimerId, after: Duration) {
        self.cmd(Command::SetTimer(id, Some(self.now.after(after))));
    }

    fn cancel_timer(&mut self, id: TimerId) {
        self.cmd(Command::SetTimer(id, None));
    }

    fn emit(&mut self, event: ClientEvent) {
        self.cmd(Command::Emit(event));
    }

    // ---- Lookups -----------------------------------------------------------

    /// Whether perception from `who` is accepted: they are in the session,
    /// and the session has not begun to stop.
    fn hears(&self, who: ParticipantId) -> bool {
        matches!(
            self.state.lifecycle,
            Lifecycle::Running | Lifecycle::Draining
        ) && self.state.participants.contains(&who)
    }

    fn turn_index(&self, turn: TurnId) -> Option<usize> {
        self.state.turns.iter().position(|open| open.id == turn)
    }

    fn turn_of(&self, who: ParticipantId) -> Option<usize> {
        self.state.turns.iter().position(|open| open.who == who)
    }

    fn generation_index(&self, generation: GenId) -> Option<usize> {
        self.state
            .generations
            .iter()
            .position(|running| running.tag.generation == generation)
    }

    /// The response that holds the floor, if there is one.
    fn floor(&self) -> Option<usize> {
        self.state
            .generations
            .iter()
            .position(|running| running.role == GenerationRole::Floor)
    }

    // ---- Turn-taking -------------------------------------------------------

    fn speech_started(&mut self, who: ParticipantId, at: MediaTime) {
        if !self.hears(who) {
            return;
        }
        let audible = self
            .floor()
            .and_then(|index| self.state.generations.get(index))
            .is_some_and(Generation::is_audible);
        let turn_max = self.config.timing.turn_max;
        let mut continued = None;
        if let Some(index) = self.turn_of(who) {
            let Some(open) = self.state.turns.get_mut(index) else {
                return;
            };
            // Edges are ordered by where they are in the audio, not by
            // when they arrive: a start from before the speech that has
            // already ended is a repeat.
            if open.speaking || at < open.span.end {
                return;
            }
            if self.now.since(open.begun) >= turn_max {
                // The turn has run out, and is only waiting for its last
                // words. What is said now is a new turn.
                self.close_turn(index);
            } else {
                // The speaker went on before the turn ended: it is still
                // the same turn, and whatever was decided about its end
                // is void.
                open.speaking = true;
                open.voiced_from = at;
                open.estimate = None;
                open.ended = None;
                if !audible {
                    // Nothing is being spoken over now, so this is not an
                    // aside, whatever the speech before it was.
                    open.aside = None;
                }
                let turn = open.id;
                self.cancel_timer(TimerId::Eou(turn));
                continued = Some(turn);
            }
        }
        let turn = if let Some(turn) = continued {
            turn
        } else {
            let turn = self.state.next_turn;
            let open = OpenTurn::begin(turn, who, at, self.now);
            if self.state.turns.push(open).is_err() {
                // Too many people are speaking at once. This speech is not
                // accepted.
                return;
            }
            self.state.next_turn = turn.next();
            self.set_timer(TimerId::TurnMax(turn), turn_max);
            turn
        };
        self.emit(ClientEvent::UserSpeaking {
            who,
            turn,
            speaking: true,
        });
        self.overlap_began(turn);
    }

    fn speech_stopped(&mut self, who: ParticipantId, at: MediaTime) {
        let Some(open) = self
            .turn_of(who)
            .and_then(|index| self.state.turns.get_mut(index))
        else {
            return;
        };
        // An end from before the stretch of speech that is under way is a
        // repeat.
        if !open.speaking || at < open.voiced_from {
            return;
        }
        open.speaking = false;
        open.quiet_since = self.now;
        open.span.end = open.span.end.max(at);
        open.estimate = None;
        let turn = open.id;
        let undecided = open.overlap.is_some();
        self.emit(ClientEvent::UserSpeaking {
            who,
            turn,
            speaking: false,
        });
        self.cmd(Command::Sense(SenseCmd::EstimateEou { turn, who }));
        if undecided {
            self.decide_overlap(turn, None);
        }
        self.decide_turn(turn);
    }

    /// Ends the open turn at `index` with the words it has, whatever it
    /// was waiting for.
    fn close_turn(&mut self, index: usize) {
        let Some(open) = self.state.turns.remove(index) else {
            return;
        };
        if let Some(overlap) = open.overlap {
            // Nobody decided that its speech takes the floor.
            self.unduck(overlap.generation);
        }
        self.turn_ended(&open, None);
    }

    fn estimated(&mut self, turn: TurnId, p: Prob) {
        let Some(open) = self
            .turn_index(turn)
            .and_then(|index| self.state.turns.get_mut(index))
        else {
            return;
        };
        // An estimate for speech that has since gone on, or for a turn
        // that is already over, is stale.
        if open.speaking || open.ended.is_some() {
            return;
        }
        open.estimate = Some(p);
        self.decide_turn(turn);
    }

    /// Asks the turn policy whether a quiet turn is over.
    fn decide_turn(&mut self, turn: TurnId) {
        let Some(open) = self
            .turn_index(turn)
            .and_then(|index| self.state.turns.get(index))
        else {
            return;
        };
        if open.speaking || open.ended.is_some() {
            return;
        }
        let input = TurnInput {
            estimate: open.estimate,
            silence: self.now.since(open.quiet_since),
            speech: open.span.len(),
        };
        match self.policies.turn().decide(&input) {
            TurnDecision::End => self.turn_is_over(turn),
            TurnDecision::Wait(more) => self.set_timer(TimerId::Eou(turn), more),
        }
    }

    /// Marks a turn as over. It ends once its words have arrived, or have
    /// had their time to.
    fn turn_is_over(&mut self, turn: TurnId) {
        let timing = self.config.timing;
        let Some(open) = self
            .turn_index(turn)
            .and_then(|index| self.state.turns.get_mut(index))
        else {
            return;
        };
        open.ended = Some(self.now);
        let who = open.who;
        // With its words in, the turn ends when this step settles, and its
        // timers are cancelled there.
        if open.lacks_transcript(&timing) {
            self.cmd(Command::Stt(SttCmd::Finalize { who }));
            self.set_timer(TimerId::Eou(turn), timing.transcript_wait);
        }
    }

    /// A turn has gone on for too long, or the end of its speech was never
    /// reported. It ends with the words it has and those still on their
    /// way.
    fn turn_ran_out(&mut self, turn: TurnId) {
        let turn_max = self.config.timing.turn_max;
        let Some(open) = self
            .turn_index(turn)
            .and_then(|index| self.state.turns.get_mut(index))
        else {
            return;
        };
        // The timer only wakes the session. Whether the turn has run out
        // is read from the state.
        if self.now.since(open.begun) < turn_max || open.ended.is_some() {
            return;
        }
        let overlap = open.overlap.take();
        if open.speaking {
            // Nobody said where the speech ends, so it is taken to have
            // lasted until now, and the transcriber is asked for the rest.
            let spoken = self.now.since(open.quiet_since).as_millis();
            let spoken = MediaDur::from_millis(u32::try_from(spoken).unwrap_or(u32::MAX));
            open.span.end = open.span.end.after(spoken);
            open.speaking = false;
            open.quiet_since = self.now;
            let who = open.who;
            self.emit(ClientEvent::UserSpeaking {
                who,
                turn,
                speaking: false,
            });
        }
        if let Some(overlap) = overlap {
            // Nobody decided that this speech takes the floor.
            self.unduck(overlap.generation);
        }
        self.turn_is_over(turn);
    }

    fn transcribed(&mut self, who: ParticipantId, span: MediaSpan, words: &str) {
        if !self.hears(who) {
            return;
        }
        let open = self
            .turn_of(who)
            .and_then(|index| self.state.turns.get_mut(index))
            .filter(|open| open.holds(span));
        match open {
            Some(open) => open.hear(span, words),
            // Words with no open turn to belong to: the listener missed the
            // speech, or the turn ended before its transcript came. They
            // are a turn of their own, so that nothing said is lost.
            None => self.typed_or_late(who, span, words),
        }
    }

    fn typed(&mut self, who: ParticipantId, words: &str) {
        if self.hears(who) {
            self.typed_or_late(who, MediaSpan::default(), words);
        }
    }

    /// A turn that begins and ends in one step: typed text, or words that
    /// arrive with no open turn to join.
    fn typed_or_late(&mut self, who: ParticipantId, span: MediaSpan, words: &str) {
        let words = words.trim();
        if words.is_empty() {
            return;
        }
        let turn = self.state.next_turn;
        self.state.next_turn = turn.next();
        self.commit_turn(turn, who, span, words);
    }

    // ---- Speech over the agent's speech ------------------------------------

    /// A turn's speaker has started to speak. If an agent is speaking, its
    /// volume is lowered, and whether it keeps the floor is decided later.
    fn overlap_began(&mut self, turn: TurnId) {
        let Some(index) = self.floor() else {
            return;
        };
        let Some(running) = self.state.generations.get_mut(index) else {
            return;
        };
        if !running.is_audible() {
            return;
        }
        let tag = running.tag;
        let lowered = running.playback == Playback::Ducked;
        running.playback = Playback::Ducked;
        let Some(open) = self
            .turn_index(turn)
            .and_then(|index| self.state.turns.get_mut(index))
        else {
            return;
        };
        if open.overlap.is_some() {
            return;
        }
        open.overlap = Some(Overlap {
            generation: tag.generation,
            since: self.now,
        });
        let who = open.who;
        if !lowered {
            self.cmd(Command::Playout(tag, PlayoutCmd::Duck));
        }
        self.cmd(Command::Sense(SenseCmd::ClassifyOverlap { turn, who }));
        self.decide_overlap(turn, None);
    }

    fn judged(&mut self, turn: TurnId, verdict: OverlapVerdict) {
        let Some(open) = self
            .turn_index(turn)
            .and_then(|index| self.state.turns.get_mut(index))
        else {
            return;
        };
        if open.overlap.is_some() {
            self.decide_overlap(turn, Some(verdict));
        } else if verdict == OverlapVerdict::BargeIn && open.aside.take().is_some() {
            // The verdict came after the wait for it ran out, and the turn
            // had been taken for an aside. While the turn is open it is
            // not too late: the speaker wanted the floor.
            self.interrupt(EpochCause::BargeIn);
        }
    }

    /// Asks the interruption policy what speech over the agent's does.
    fn decide_overlap(&mut self, turn: TurnId, verdict: Option<OverlapVerdict>) {
        let Some(open) = self
            .turn_index(turn)
            .and_then(|index| self.state.turns.get_mut(index))
        else {
            return;
        };
        let Some(overlap) = open.overlap else {
            return;
        };
        let input = OverlapInput {
            verdict,
            speaking: open.speaking,
            elapsed: self.now.since(overlap.since),
        };
        match self.policies.interruption().decide(&input) {
            OverlapDecision::Wait(more) => self.set_timer(TimerId::Overlap(turn), more),
            OverlapDecision::Interrupt => {
                open.overlap = None;
                open.aside = None;
                self.cancel_timer(TimerId::Overlap(turn));
                // Only the response that was spoken over is stopped. If it
                // has ended by itself, there is nothing to interrupt.
                if self.generation_index(overlap.generation).is_some() {
                    self.interrupt(EpochCause::BargeIn);
                }
            }
            OverlapDecision::Resume => {
                open.overlap = None;
                open.aside = Some(match verdict {
                    Some(OverlapVerdict::Noise) => Dismissal::Noise,
                    _ => Dismissal::Backchannel,
                });
                self.cancel_timer(TimerId::Overlap(turn));
                self.unduck(overlap.generation);
            }
        }
    }

    /// Restores a response's volume, unless someone else is still speaking
    /// over it.
    fn unduck(&mut self, generation: GenId) {
        let spoken_over = self.state.turns.iter().any(|open| {
            open.overlap
                .is_some_and(|overlap| overlap.generation == generation)
        });
        if spoken_over {
            return;
        }
        let Some(running) = self
            .generation_index(generation)
            .and_then(|index| self.state.generations.get_mut(index))
        else {
            return;
        };
        if running.playback == Playback::Ducked {
            running.playback = Playback::Playing;
            let tag = running.tag;
            self.cmd(Command::Playout(tag, PlayoutCmd::Unduck));
        }
    }

    // ---- Responses ---------------------------------------------------------

    fn op(&mut self, tag: OpTag, op: &OpEvent) {
        // The one place where the results of work that no longer counts
        // are dropped.
        let Some(index) = self
            .state
            .generations
            .iter()
            .position(|running| running.tag == tag)
        else {
            return;
        };
        let Some(running) = self.state.generations.get_mut(index) else {
            return;
        };
        if let GenerationRole::Delegated(call) = running.role {
            self.delegated_op(index, call, op);
            return;
        }
        match op {
            OpEvent::LlmDelta { text } => {
                if running.is_written() {
                    return;
                }
                let first = running.planned.is_empty();
                running.planned.push_str(text.as_str());
                if first {
                    self.watch(index);
                }
                self.pump(index);
            }
            OpEvent::LlmDone { .. } => {
                if running.is_written() {
                    return;
                }
                running.source = Source::Written;
                if running.planned.trim().is_empty() {
                    // The model had nothing to say.
                    self.end_generation(index, false);
                } else {
                    self.watch(index);
                    self.pump(index);
                }
            }
            OpEvent::TtsSegment {
                text_end,
                audio_end,
                last,
            } => {
                let text_end = usize::try_from(*text_end).unwrap_or(usize::MAX);
                running.synthesized = running.synthesized.max(text_end.min(running.released));
                running.audio = running.audio.max(*audio_end);
                if *last && running.synthesis == Synthesis::Closing {
                    running.synthesis = Synthesis::Done;
                }
                self.pump(index);
            }
            OpEvent::Played {
                handed,
                heard_text,
                drained,
                ..
            } => {
                running.handed = running.handed.max(*handed);
                running.hear(usize::try_from(*heard_text).unwrap_or(usize::MAX));
                let played_out =
                    *drained && running.synthesis == Synthesis::Done && running.is_audible();
                if played_out {
                    self.end_generation(index, false);
                } else {
                    self.pump(index);
                }
            }
            OpEvent::Failed { part, .. } => self.failed(index, *part),
            OpEvent::ToolProposed { tool, args } => self.proposed(index, *tool, args.as_str()),
        }
    }

    /// Hands a response's text on to synthesis as far as it is ready and
    /// the listener is close enough behind, and tells the language model
    /// to hold on when too much is waiting.
    fn pump(&mut self, index: usize) {
        let Some(running) = self.state.generations.get_mut(index) else {
            return;
        };
        let tag = running.tag;
        if running.may_release(self.config.timing.speech_credit) {
            let pending = running.pending();
            let ready = self.policies.chunker().ready(pending, running.is_written());
            // A policy's answer is not trusted further than it can be
            // checked.
            let chunk = pending.get(..ready).unwrap_or("");
            // White space alone is nothing to say. It is held, and goes
            // with what follows: synthesis is not started for nothing.
            if says_something(chunk) {
                let text = self.out.text.push(chunk);
                self.out
                    .cmds
                    .push(Command::Tts(tag, TtsCmd::Speak { text }));
                running.released += ready;
            }
        }
        let spoken = running.released > 0;
        // White space after the last words is never handed over, so the
        // response has said everything once nothing else is left.
        if running.is_written()
            && spoken
            && running.synthesis == Synthesis::Open
            && !says_something(running.pending())
        {
            running.synthesis = Synthesis::Closing;
            self.out.cmds.push(Command::Tts(tag, TtsCmd::Finish));
        }

        let waiting = running.planned.len() - running.released;
        let limit = self.config.limits.pending_text;
        match running.source {
            // The model goes on writing though it was told to wait. It is
            // stopped, so that the text held for it stays bounded, and
            // what it wrote is said.
            Source::Writing | Source::Paused if waiting > limit.saturating_mul(2) => {
                running.source = Source::Written;
                self.out.cmds.push(Command::Llm(tag, LlmCmd::Cancel));
            }
            Source::Writing if waiting > limit => {
                running.source = Source::Paused;
                self.out.cmds.push(Command::Llm(tag, LlmCmd::Pause));
            }
            Source::Paused if waiting <= limit / 2 => {
                running.source = Source::Writing;
                self.out.cmds.push(Command::Llm(tag, LlmCmd::Resume));
            }
            _ => {}
        }
    }

    /// A part working on a response has failed.
    fn failed(&mut self, index: usize, part: Part) {
        let Some(running) = self.state.generations.get_mut(index) else {
            return;
        };
        match part {
            // The model stopped early. What it wrote is still said.
            Part::Think if !running.planned.trim().is_empty() => {
                if !running.is_written() {
                    running.source = Source::Written;
                }
                self.pump(index);
            }
            Part::Think => self.end_generation(index, false),
            // Nothing more can be heard of this response.
            Part::Speak | Part::Play => self.interrupt(EpochCause::Failure),
        }
    }

    /// Ends a response. `cut` says that it is stopped before it has played
    /// to its end, so the parts still working on it are told to stop.
    fn end_generation(&mut self, index: usize, cut: bool) {
        let Some(running) = self.state.generations.remove(index) else {
            return;
        };
        let tag = running.tag;
        if cut {
            if running.is_audible() {
                let fade_ms = self.config.timing.stop_fade_ms;
                self.cmd(Command::Playout(tag, PlayoutCmd::Stop { fade_ms }));
            }
            if running.released > 0 && running.synthesis != Synthesis::Done {
                self.cmd(Command::Tts(tag, TtsCmd::Cancel));
            }
            if matches!(running.source, Source::Writing | Source::Paused) {
                self.cmd(Command::Llm(tag, LlmCmd::Cancel));
            }
        }
        self.cancel_timer(TimerId::Gen(tag.generation));
        if !self.state.generations.iter().any(Generation::is_audible) {
            self.cancel_timer(TimerId::Checkpoint);
        }
        self.utterance_ended(&running, !cut);
        self.asker_ended(tag.generation, cut);
        self.emit(ClientEvent::Agent {
            agent: running.agent,
            generation: Some(tag.generation),
            activity: Activity::Listening,
        });
    }

    /// An agent's speech is over. What was heard of it enters the
    /// conversation, once, here and nowhere else.
    fn utterance_ended(&mut self, running: &Generation, played_out: bool) {
        let heard = running.heard_text(played_out);
        if heard.is_empty() {
            return;
        }
        let truncated = running.is_cut_at(heard);
        // Speech that was heard to the end answers what its response was
        // computed from. Speech that was cut off, or that the application
        // asked for, answers nothing.
        let answers = (!truncated && running.source != Source::Given).then_some(running.context);
        let text = self.out.text.push(heard);
        self.fact(Fact::UtteranceCommitted {
            generation: running.tag.generation,
            agent: running.agent,
            answers,
            text,
            truncated,
        });
    }

    /// Ends every response that holds the floor, and advances the epoch if
    /// there was one. The epoch moves here and nowhere else.
    ///
    /// Work on a tool call is not touched: it does not hold the floor, and
    /// its call outlives the response that asked for it.
    fn interrupt(&mut self, cause: EpochCause) {
        let mut ended = false;
        while let Some(index) = self.floor() {
            self.end_generation(index, true);
            ended = true;
        }
        if ended {
            let to = self.state.epoch.next();
            self.fact(Fact::EpochAdvanced { to, cause });
            // The calls those responses had not sent yet ended with them.
            self.resolve_batches();
        }
    }

    /// Records how far playback has got, about once a second.
    fn checkpoint(&mut self) {
        let mut playing = false;
        for index in 0..self.state.generations.len() {
            let Some(running) = self.state.generations.get_mut(index) else {
                break;
            };
            if !running.is_audible() {
                continue;
            }
            playing = true;
            let (from, to) = (running.recorded, running.heard);
            if to <= from {
                continue;
            }
            running.recorded = to;
            let (generation, agent) = (running.tag.generation, running.agent);
            let heard = self
                .out
                .text
                .push(running.planned.get(from..to).unwrap_or(""));
            self.fact(Fact::UtteranceProgress {
                generation,
                agent,
                heard,
            });
            // The response is making progress, so it is not stalled.
            self.watch(index);
        }
        if playing {
            self.set_timer(TimerId::Checkpoint, self.config.timing.checkpoint_every);
        }
    }

    // ---- Timers and control ------------------------------------------------

    fn timer_fired(&mut self, id: TimerId) {
        match id {
            TimerId::Eou(turn) => self.decide_turn(turn),
            TimerId::Overlap(turn) => self.decide_overlap(turn, None),
            TimerId::TurnMax(turn) => self.turn_ran_out(turn),
            TimerId::Gen(generation) => {
                let stalled = self
                    .generation_index(generation)
                    .and_then(|index| self.state.generations.get(index))
                    .is_some_and(|running| {
                        running.role == GenerationRole::Floor && self.now >= running.stall_at
                    });
                if stalled {
                    self.interrupt(EpochCause::Failure);
                }
            }
            TimerId::Checkpoint => self.checkpoint(),
            // These only need the session to look at the time, which
            // `settle` does after every event.
            TimerId::Drain
            | TimerId::Grace
            | TimerId::ToolTimeout(_)
            | TimerId::Rollback(_)
            | TimerId::StageAck => {}
        }
    }

    fn control(&mut self, input: &ControlInput) {
        match input {
            ControlInput::Join { who } => {
                let known = self.state.participants.contains(who);
                if !known && !self.state.participants.is_full() {
                    self.fact(Fact::ParticipantJoined { who: *who });
                }
            }
            ControlInput::Leave { who } => self.left(*who),
            ControlInput::Close { reason } => self.close_asked(*reason),
            ControlInput::Interrupt => self.interrupt(EpochCause::Interrupted),
            ControlInput::Say { text } => self.say(text.as_str()),
            ControlInput::Detached => self.state.attached = false,
            ControlInput::Attached => self.state.attached = true,
            ControlInput::Handoff { to, context } => self.handoff_asked(*to, context),
            ControlInput::StageReady => self.stage_ready(),
            // Losing the lease is handled before the step is routed.
            ControlInput::LeaseLost => {}
        }
    }

    fn left(&mut self, who: ParticipantId) {
        if !self.state.participants.contains(&who) {
            return;
        }
        // What they were saying ends with the words that arrived.
        if let Some(index) = self.turn_of(who) {
            self.close_turn(index);
        }
        self.cmd(Command::Stt(SttCmd::Close { who }));
        self.fact(Fact::ParticipantLeft { who });
    }

    /// Has the agent that holds the floor say `text`, in place of whatever
    /// it was saying.
    fn say(&mut self, text: &str) {
        let running = self.state.lifecycle == Lifecycle::Running;
        // While the floor is changing hands, nobody holds it.
        if !running || self.state.handoff.is_some() || text.trim().is_empty() {
            return;
        }
        self.interrupt(EpochCause::Interrupted);
        let front = self.state.front;
        let Some(index) = self.begin_generation(front, GenerationRole::Floor, Source::Given) else {
            return;
        };
        self.watch(index);
        if let Some(running) = self.state.generations.get_mut(index) {
            running.planned.push_str(text);
        }
        self.pump(index);
    }

    /// Adds a response, and returns where it is. `None` if as many
    /// responses are under way as there may be.
    fn begin_generation(
        &mut self,
        agent: AgentId,
        role: GenerationRole,
        source: Source,
    ) -> Option<usize> {
        let generation = self.state.next_gen;
        let tag = OpTag {
            generation,
            epoch: self.state.epoch,
        };
        let context = self.state.conversation.version();
        let reserve = self.config.limits.response_text;
        let running = Generation::begin(tag, agent, role, context, source, reserve);
        self.state.generations.push(running).ok()?;
        self.state.next_gen = generation.next();
        Some(self.state.generations.len() - 1)
    }

    /// Gives a response a full `generation_max` from now to make progress.
    /// The timer only wakes the session: whether the response has stalled
    /// is read from the state when it fires.
    fn watch(&mut self, index: usize) {
        let stall_at = self.now.after(self.config.timing.generation_max);
        let Some(running) = self.state.generations.get_mut(index) else {
            return;
        };
        running.stall_at = stall_at;
        let generation = running.tag.generation;
        self.cmd(Command::SetTimer(TimerId::Gen(generation), Some(stall_at)));
    }

    fn close_asked(&mut self, reason: CloseReason) {
        if self.state.lifecycle != Lifecycle::Running {
            return;
        }
        let timing = self.config.timing;
        let drain_until = self.now.after(timing.drain);
        let grace_until = drain_until.after(timing.grace);
        self.state.closing = Some(Closing {
            reason,
            drain_until,
            grace_until,
        });
        self.fact(Fact::LifecycleChanged {
            to: Lifecycle::Draining,
            reason: Some(reason),
        });
        // Two timers, so that losing one still ends the session on time.
        self.cmd(Command::SetTimer(TimerId::Drain, Some(drain_until)));
        self.cmd(Command::SetTimer(TimerId::Grace, Some(grace_until)));
        self.emit(ClientEvent::Lifecycle(Lifecycle::Draining));
    }

    // ---- Ending turns ------------------------------------------------------

    /// A user turn is over. It is committed to the conversation or
    /// dismissed, once, here and nowhere else. `forced` gives the reason
    /// when the turn is dismissed whatever it holds.
    fn turn_ended(&mut self, open: &OpenTurn, forced: Option<Dismissal>) {
        for id in [
            TimerId::Eou(open.id),
            TimerId::Overlap(open.id),
            TimerId::TurnMax(open.id),
        ] {
            self.cancel_timer(id);
        }
        let words = open.text.trim();
        let reason = forced
            .or(open.aside)
            .or_else(|| words.is_empty().then_some(Dismissal::Empty));
        match reason {
            Some(reason) => self.fact(Fact::TurnDismissed {
                turn: open.id,
                who: open.who,
                reason,
            }),
            None => self.commit_turn(open.id, open.who, open.span, words),
        }
    }

    fn commit_turn(&mut self, turn: TurnId, who: ParticipantId, span: MediaSpan, words: &str) {
        let addressed = self.policies.addressee().addressed(&AddresseeInput {
            who,
            text: words,
            humans: self.state.participants.len(),
        });
        let text = self.out.text.push(words);
        self.fact(Fact::TurnCommitted {
            turn,
            who,
            span,
            text,
            addressed,
        });
    }

    // ---- The rules that start things ---------------------------------------

    /// Applies, in a fixed order, the rules that follow from the state and
    /// not from any one event.
    ///
    /// Most steps of most sessions have no tool call to look at, no floor
    /// changing hands, no reply to start and nothing closing. Each of
    /// those rules is asked for only when it has something to look at, so
    /// that the steps a session takes most often stay short.
    fn settle(&mut self) {
        self.end_turns();
        if !self.state.calls.is_empty() || !self.state.batches.is_empty() {
            self.advance_calls();
        }
        if self.state.handoff.is_some() {
            self.advance_stage();
        }
        if self.state.reply_owed() {
            self.start_reply();
        }
        self.start_playback();
        if self.state.closing.is_some() {
            self.advance_lifecycle();
        }
    }

    /// Ends every turn that can end.
    fn end_turns(&mut self) {
        let timing = self.config.timing;
        let mut index = 0;
        while let Some(open) = self.state.turns.get(index) {
            if open.is_over(self.now, &timing) {
                if let Some(open) = self.state.turns.remove(index) {
                    self.turn_ended(&open, None);
                }
            } else {
                index += 1;
            }
        }
    }

    /// Starts the response that is owed, if one is and it can start.
    fn start_reply(&mut self) {
        // A session that is closing starts nothing, and while the floor is
        // changing hands there is nobody to respond.
        if self.state.lifecycle != Lifecycle::Running || self.state.handoff.is_some() {
            return;
        }
        let Some((_, cause)) = self.state.owed.filter(|_| self.state.reply_owed()) else {
            return;
        };
        if let Some(running) = self
            .floor()
            .and_then(|index| self.state.generations.get(index))
        {
            let waits = running.is_audible()
                || running.source == Source::Given
                || matches!(cause, LlmCause::Continuation { .. });
            if waits {
                // It is being heard, or the application asked for it to be
                // said, or what is owed is only the news of a tool: that
                // never takes the floor from a response. The next response
                // waits for this one.
                return;
            }
            // Nobody has heard any of it, and the user has said more
            // since.
            self.interrupt(EpochCause::Superseded);
        }
        // Cutting a response off resolves the calls it asked for, and then
        // their results are what the next response is for.
        let Some((_, cause)) = self.state.owed else {
            return;
        };
        let front = self.state.front;
        let Some(index) = self.begin_generation(front, GenerationRole::Floor, Source::Writing)
        else {
            return;
        };
        self.watch(index);
        let Some(running) = self.state.generations.get(index) else {
            return;
        };
        let (tag, agent, context) = (running.tag, running.agent, running.context);
        self.state.started = context;
        let start = LlmCmd::Start {
            agent,
            context,
            cause,
        };
        self.cmd(Command::Llm(tag, start));
        self.emit(ClientEvent::Agent {
            agent,
            generation: Some(tag.generation),
            activity: Activity::Thinking,
        });
    }

    /// Lets the response that holds the floor be heard, once it has
    /// something to say and nobody is in the middle of a turn.
    fn start_playback(&mut self) {
        if !self.state.turns.is_empty() {
            return;
        }
        let Some(running) = self
            .floor()
            .and_then(|index| self.state.generations.get_mut(index))
        else {
            return;
        };
        if running.is_audible() || running.released == 0 {
            return;
        }
        running.playback = Playback::Playing;
        let (tag, agent) = (running.tag, running.agent);
        self.cmd(Command::Playout(tag, PlayoutCmd::Start));
        self.set_timer(TimerId::Checkpoint, self.config.timing.checkpoint_every);
        self.emit(ClientEvent::Agent {
            agent,
            generation: Some(tag.generation),
            activity: Activity::Speaking,
        });
    }

    /// Moves a closing session on when it has finished what it was doing,
    /// or has run out of time to.
    fn advance_lifecycle(&mut self) {
        let Some(closing) = self.state.closing else {
            return;
        };
        let reason = Some(closing.reason);
        if self.state.lifecycle == Lifecycle::Draining {
            let busy = !self.state.generations.is_empty() || !self.state.calls.is_empty();
            if busy && self.now < closing.drain_until {
                return;
            }
            self.fact(Fact::LifecycleChanged {
                to: Lifecycle::Closing,
                reason,
            });
            self.interrupt(EpochCause::Closing);
            while let Some(open) = self.state.turns.remove(0) {
                self.turn_ended(&open, Some(Dismissal::Closed));
            }
            for index in 0..self.state.participants.len() {
                if let Some(&who) = self.state.participants.get(index) {
                    self.cmd(Command::Stt(SttCmd::Close { who }));
                }
            }
            self.stop_calls();
            for id in [TimerId::Checkpoint, TimerId::Drain] {
                self.cancel_timer(id);
            }
        }
        if self.state.lifecycle != Lifecycle::Closing {
            return;
        }
        // What is left are calls that may have taken effect. Their results
        // are waited for, and no longer than the session has.
        if !self.state.calls.is_empty() && self.now < closing.grace_until {
            return;
        }
        while let Some(open) = self.state.calls.first() {
            let call = open.id;
            self.call_finished(call, ToolOutcome::InDoubt, tools::IN_DOUBT);
        }
        self.resolve_batches();
        self.cancel_timer(TimerId::Grace);
        self.fact(Fact::LifecycleChanged {
            to: Lifecycle::Closed,
            reason,
        });
        self.emit(ClientEvent::Lifecycle(Lifecycle::Closed));
    }

    // ---- The floor changing hands ------------------------------------------

    /// Another agent is to take the floor. The agent that has it stops,
    /// and nobody responds until the parts that serve the new one are
    /// ready.
    fn handoff_asked(&mut self, to: AgentId, context: &HandoffContext) {
        let running = self.state.lifecycle == Lifecycle::Running;
        let known = self.config.agent(to).is_some();
        // An agent that works on tool calls for others never speaks.
        let works = self
            .config
            .tools
            .iter()
            .any(|tool| tool.executor == Executor::Agent(to));
        if !running || !known || works || to == self.state.front || self.state.handoff.is_some() {
            return;
        }
        self.fact(Fact::HandoffRequested { to });
        self.interrupt(EpochCause::Handoff);
        let note = match context {
            HandoffContext::Full => None,
            HandoffContext::Summary(summary) => Some(summary.as_str()),
            HandoffContext::None => Some(""),
        };
        if let Some(note) = note {
            let text = self.out.text.push(note);
            self.fact(Fact::NoticeAppended {
                kind: NoticeKind::Handoff,
                text,
            });
        }
        let until = self.now.after(self.config.timing.stage_wait);
        self.state.stage_until = Some(until);
        self.cmd(Command::SetTimer(TimerId::StageAck, Some(until)));
    }

    /// The parts that serve the agent that was asked to take the floor
    /// are ready: it has the floor.
    fn stage_ready(&mut self) {
        if let Some(agent) = self.state.handoff {
            self.activate(agent);
        }
    }

    fn activate(&mut self, agent: AgentId) {
        self.state.stage_until = None;
        self.cancel_timer(TimerId::StageAck);
        self.fact(Fact::AgentActivated { agent });
        self.emit(ClientEvent::Agent {
            agent,
            generation: None,
            activity: Activity::Listening,
        });
    }

    /// Gives the floor back to the agent that had it, when the one that
    /// was asked to take it has not become ready in time.
    fn advance_stage(&mut self) {
        let late = self
            .state
            .stage_until
            .is_some_and(|until| self.now >= until);
        if self.state.handoff.is_some() && late {
            self.activate(self.state.front);
        }
    }

    // ---- After a restart ---------------------------------------------------

    /// Deals with what a session that was rebuilt from its journal lost
    /// with the process: the responses and tool calls that were under way.
    pub(super) fn recover(&mut self) {
        self.state.recovering = false;
        // Whatever still arrives for the work of the session's earlier
        // life is stale.
        let to = self.state.epoch.next();
        self.fact(Fact::EpochAdvanced {
            to,
            cause: EpochCause::Recovery,
        });
        let mut lost = self.state.reply_owed();

        // The speech that was being heard was heard at least as far as
        // was last recorded.
        if let Some((generation, agent)) = self.state.speech {
            lost = true;
            let text = self.out.text.push(self.state.speech_text.trim_end());
            self.fact(Fact::UtteranceCommitted {
                generation,
                agent,
                answers: None,
                text,
                truncated: true,
            });
        }

        // The responses that asked for the calls are gone.
        for batch in &mut self.state.batches {
            batch.over = true;
        }
        let running = self.state.lifecycle == Lifecycle::Running;
        let mut index = 0;
        while let Some(open) = self.state.calls.get(index).copied() {
            lost = true;
            let effect = self.config.tool(open.tool).map(|spec| spec.effect);
            let cleared = open.state != CallState::Proposed;
            // A call that was cleared may have been sent, whether or not
            // the record of that survived.
            if !cleared {
                self.call_finished(open.id, ToolOutcome::Failed, tools::LOST);
                continue;
            }
            let harmless = matches!(effect, Some(Effect::Pure | Effect::Idempotent));
            if harmless && running && open.attempt < tools::MOST_RETRIES {
                // Sending it again does no harm. `settle` does, if the
                // session is still running by then.
                index += 1;
                continue;
            }
            self.lose(&open);
        }

        if lost {
            let text = self.out.text.push(tools::RECOVERED);
            self.fact(Fact::NoticeAppended {
                kind: NoticeKind::Recovered,
                text,
            });
        }
        // Whoever was to take the floor has it: every part is being set
        // up anew anyway.
        if let Some(agent) = self.state.handoff {
            self.activate(agent);
        }
        // A session that was closing has nothing left to wait for.
        if !running {
            self.state.closing = Some(Closing {
                reason: self.state.closed_for.unwrap_or(CloseReason::Failure),
                drain_until: self.now,
                grace_until: self.now,
            });
        }
    }

    // ---- Tool calls --------------------------------------------------------

    fn call(&self, call: ToolCallId) -> Option<Call> {
        self.state
            .calls
            .iter()
            .find(|open| open.id == call)
            .copied()
    }

    fn call_mut(&mut self, call: ToolCallId) -> Option<&mut Call> {
        self.state.calls.iter_mut().find(|open| open.id == call)
    }

    /// A response asks for a tool call. The request enters the
    /// conversation, and the call waits for its gate.
    fn proposed(&mut self, index: usize, tool: ToolId, args: &str) {
        let Some(running) = self.state.generations.get(index) else {
            return;
        };
        // Tools are called by the agent that holds the floor, while its
        // model writes, and only the ones it was given.
        let may = self
            .config
            .agent(running.agent)
            .is_some_and(|agent| agent.tools.contains(&tool));
        if running.role != GenerationRole::Floor || running.is_written() || !may {
            return;
        }
        let generation = running.tag.generation;
        let batch = self
            .state
            .batches
            .iter()
            .find(|open| open.by == generation)
            .map(|open| open.id);
        // A call there is no room for is not accepted.
        if self.state.calls.is_full() || (batch.is_none() && self.state.batches.is_full()) {
            return;
        }
        let call = self.state.next_call;
        let batch = batch.unwrap_or(self.state.next_batch);
        let args = self.out.text.push(args);
        self.fact(Fact::ToolProposed {
            call,
            batch,
            generation,
            tool,
            args,
        });
        // The model is making progress.
        self.watch(index);
    }

    /// The response `generation` has ended. Its calls no longer wait for
    /// its speech, and if it was cut off, those not sent yet are
    /// withdrawn: what it was doing is not wanted any more.
    fn asker_ended(&mut self, generation: GenId, cut: bool) {
        let Some(batch) = self
            .state
            .batches
            .iter_mut()
            .find(|open| open.by == generation)
        else {
            return;
        };
        batch.over = true;
        let batch = batch.id;
        if !cut {
            return;
        }
        let mut index = 0;
        while let Some(open) = self.state.calls.get(index) {
            if open.batch == batch && !open.is_sent() {
                let call = open.id;
                self.call_finished(call, ToolOutcome::Cancelled, tools::WITHDRAWN);
            } else {
                index += 1;
            }
        }
    }

    /// Every fact up to `seq` is stored durably.
    fn stored(&mut self, seq: Seq) {
        // Nothing is stored that was not emitted.
        let emitted = Seq(self.state.next_seq.get().saturating_sub(1));
        self.state.durable = self.state.durable.max(seq.min(emitted));
    }

    /// News of a call from its tool.
    fn tool(&mut self, call: ToolCallId, news: &ToolEvent) {
        // A call that has ended is not in the table: this is where a
        // second result is dropped.
        let Some(open) = self.call(call) else {
            return;
        };
        let external = self
            .config
            .tool(open.tool)
            .is_some_and(|spec| spec.executor == Executor::External);
        if !external || !open.is_sent() {
            return;
        }
        match news {
            ToolEvent::Completed { result } => {
                self.call_finished(call, ToolOutcome::Completed, result.as_str());
            }
            ToolEvent::Failed { message, .. } => {
                self.call_finished(call, ToolOutcome::Failed, message.as_str());
            }
            ToolEvent::RolledBack => {
                if open.state == CallState::Cancelling {
                    self.call_finished(call, ToolOutcome::Cancelled, tools::UNDONE);
                }
            }
        }
    }

    /// A tool call ends. Its result enters the conversation, once, here
    /// and nowhere else.
    fn call_finished(&mut self, call: ToolCallId, outcome: ToolOutcome, result: &str) {
        if self.call(call).is_none() {
            return;
        }
        // An agent that was working on it has nothing left to work on.
        let worker = self
            .state
            .generations
            .iter()
            .position(|running| running.role == GenerationRole::Delegated(call));
        if let Some(running) = worker.and_then(|index| self.state.generations.remove(index))
            && !running.is_written()
        {
            self.cmd(Command::Llm(running.tag, LlmCmd::Cancel));
        }
        for id in [TimerId::ToolTimeout(call), TimerId::Rollback(call)] {
            self.cancel_timer(id);
        }
        let result = self.out.text.push(result);
        self.fact(Fact::ToolFinished {
            call,
            outcome,
            result,
        });
    }

    /// Moves every call on as far as it can go: through its gate, past
    /// the record that must be stored first, to its tool, and out when its
    /// time is up. Then resolves the batches that have nothing left.
    fn advance_calls(&mut self) {
        let mut index = 0;
        while let Some(open) = self.state.calls.get(index).copied() {
            let moved = match open.state {
                CallState::Proposed => self.authorize(&open),
                CallState::Authorized => self.send_when_stored(&open),
                CallState::Dispatched | CallState::Cancelling => self.check_sent(&open),
            };
            if moved == Moved::Waits {
                index += 1;
            }
        }
        self.resolve_batches();
    }

    /// Clears a call to run, once its gate is open.
    fn authorize(&mut self, open: &Call) -> Moved {
        let Some(spec) = self.config.tool(open.tool).copied() else {
            self.call_finished(open.id, ToolOutcome::Failed, tools::NOT_ALLOWED);
            return Moved::Ended;
        };
        if self.state.lifecycle != Lifecycle::Running {
            // A session that is closing starts nothing new.
            self.call_finished(open.id, ToolOutcome::Cancelled, tools::WITHDRAWN);
            return Moved::Ended;
        }
        let asker_over = self
            .state
            .batches
            .iter()
            .find(|batch| batch.id == open.batch)
            .is_none_or(|batch| batch.over);
        let input = GateInput {
            tool: open.tool,
            gate: spec.gate,
            utterance_done: asker_over,
        };
        if !self.policies.gate().open(&input) {
            if !asker_over {
                return Moved::Waits;
            }
            // Nothing the gate could be waiting for is still to come.
            self.call_finished(open.id, ToolOutcome::Failed, tools::NOT_ALLOWED);
            return Moved::Ended;
        }
        let seq = self.journal(Fact::ToolAuthorized { call: open.id });
        if spec.effect != Effect::Pure {
            // The call may change the world, so the record that it was
            // cleared has to be safe first: a crash must not forget a call
            // that may have run.
            let due = self.now.after(spec.timeout);
            if let Some(open) = self.call_mut(open.id) {
                open.barrier = Some(seq);
                open.due = Some(due);
            }
            self.cmd(Command::AwaitDurable(seq));
            self.cmd(Command::SetTimer(TimerId::ToolTimeout(open.id), Some(due)));
        }
        Moved::On
    }

    /// Ends a call that was cleared in the session's earlier life and is
    /// not sent again. It may have been sent then: whether it ran, nobody
    /// knows.
    fn lose(&mut self, open: &Call) -> Moved {
        let effect = self.config.tool(open.tool).map(|spec| spec.effect);
        let (outcome, result) = match effect {
            Some(Effect::Pure) | None => (ToolOutcome::Failed, tools::ANSWER_LOST),
            Some(Effect::Idempotent | Effect::SideEffecting) => {
                (ToolOutcome::InDoubt, tools::IN_DOUBT)
            }
        };
        self.call_finished(open.id, outcome, result);
        Moved::Ended
    }

    /// Sends a call that was cleared, once the record of that is stored.
    fn send_when_stored(&mut self, open: &Call) -> Moved {
        if self.state.lifecycle != Lifecycle::Running {
            // A call that waits for its record was never sent. One that
            // waits for nothing was cleared before the session was
            // restarted.
            if open.barrier.is_none() {
                return self.lose(open);
            }
            self.call_finished(open.id, ToolOutcome::Cancelled, tools::WITHDRAWN);
            return Moved::Ended;
        }
        if open
            .barrier
            .is_some_and(|barrier| self.state.durable < barrier)
        {
            if open.due.is_some_and(|due| self.now < due) {
                return Moved::Waits;
            }
            self.call_finished(open.id, ToolOutcome::Failed, tools::NOT_RECORDED);
            return Moved::Ended;
        }
        self.send(open)
    }

    /// Sends a call to what runs its tool.
    fn send(&mut self, open: &Call) -> Moved {
        let Some(spec) = self.config.tool(open.tool).copied() else {
            self.call_finished(open.id, ToolOutcome::Failed, tools::NOT_ALLOWED);
            return Moved::Ended;
        };
        let call = open.id;
        // A call that was sent before is sent again only after the
        // session was restarted.
        let attempt = if open.is_sent() {
            open.attempt.saturating_add(1)
        } else {
            open.attempt
        };
        match spec.executor {
            Executor::External => {
                self.fact(Fact::ToolDispatched { call, attempt });
                let args = self.out.text.push(self.state.conversation.call_args(call));
                let tool = open.tool;
                self.cmd(Command::Tool(
                    call,
                    ToolCmd::Dispatch {
                        tool,
                        args,
                        attempt,
                    },
                ));
            }
            Executor::Agent(worker) => {
                let role = GenerationRole::Delegated(call);
                let Some(index) = self.begin_generation(worker, role, Source::Writing) else {
                    self.call_finished(call, ToolOutcome::Failed, tools::NO_AGENT);
                    return Moved::Ended;
                };
                self.fact(Fact::ToolDispatched { call, attempt });
                if let Some(running) = self.state.generations.get(index) {
                    let start = LlmCmd::Start {
                        agent: worker,
                        context: running.context,
                        cause: LlmCause::Delegated { call },
                    };
                    self.cmd(Command::Llm(running.tag, start));
                }
            }
        }
        let due = self.now.after(spec.timeout);
        if let Some(open) = self.call_mut(call) {
            open.due = Some(due);
        }
        self.cmd(Command::SetTimer(TimerId::ToolTimeout(call), Some(due)));
        Moved::On
    }

    /// Looks at a call that was sent: whether its tool has had its time.
    fn check_sent(&mut self, open: &Call) -> Moved {
        let Some(due) = open.due else {
            // It was sent before the session was restarted, and whoever
            // was running it is gone. A session that is closing sends
            // nothing.
            if self.state.lifecycle != Lifecycle::Running {
                return self.lose(open);
            }
            return self.send(open);
        };
        if self.now < due {
            return Moved::Waits;
        }
        let call = open.id;
        if open.state == CallState::Cancelling {
            // The tool never confirmed that it undid the call.
            self.call_finished(call, ToolOutcome::InDoubt, tools::IN_DOUBT);
            return Moved::Ended;
        }
        let spec = self.config.tool(open.tool).copied();
        let external = spec.is_some_and(|spec| spec.executor == Executor::External);
        if external {
            self.cmd(Command::Tool(call, ToolCmd::Cancel));
        }
        let harmless = spec.is_none_or(|spec| spec.effect == Effect::Pure);
        if harmless || !external {
            self.call_finished(call, ToolOutcome::Failed, tools::TIMED_OUT);
            return Moved::Ended;
        }
        // It may have taken effect. The tool is given time to say that it
        // undid the call.
        let due = self.now.after(self.config.timing.rollback_wait);
        if let Some(open) = self.call_mut(call) {
            open.state = CallState::Cancelling;
            open.due = Some(due);
        }
        self.cmd(Command::SetTimer(TimerId::Rollback(call), Some(due)));
        Moved::On
    }

    /// Ends what can be ended of the calls of a session that is stopping:
    /// those not sent, and those that change nothing. A call that may have
    /// taken effect is left to finish.
    fn stop_calls(&mut self) {
        let mut index = 0;
        while let Some(open) = self.state.calls.get(index).copied() {
            let spec = self.config.tool(open.tool).copied();
            let harmless = spec.is_none_or(|spec| spec.effect == Effect::Pure);
            if !open.is_sent() {
                self.call_finished(open.id, ToolOutcome::Cancelled, tools::WITHDRAWN);
            } else if harmless {
                if spec.is_some_and(|spec| spec.executor == Executor::External) {
                    self.cmd(Command::Tool(open.id, ToolCmd::Cancel));
                }
                self.call_finished(open.id, ToolOutcome::Cancelled, tools::UNDONE);
            } else {
                index += 1;
            }
        }
    }

    /// Resolves every batch whose calls have all ended and that no call
    /// can be added to. A batch of which something ran is owed a
    /// response, the continuation.
    fn resolve_batches(&mut self) {
        let mut index = 0;
        while let Some(open) = self.state.batches.get(index) {
            let writing = !open.over
                && self
                    .generation_index(open.by)
                    .and_then(|index| self.state.generations.get(index))
                    .is_some_and(|running| !running.is_written());
            if open.pending == 0 && !writing {
                let batch = open.id;
                self.fact(Fact::BatchResolved { batch });
            } else {
                index += 1;
            }
        }
    }

    // ---- Work on a tool call by another agent ------------------------------

    /// A result of the work another agent does on a tool call. What it
    /// writes is not spoken: it is the call's result.
    fn delegated_op(&mut self, index: usize, call: ToolCallId, op: &OpEvent) {
        let limit = self.config.limits.pending_text.saturating_mul(2);
        let Some(running) = self.state.generations.get_mut(index) else {
            return;
        };
        match op {
            OpEvent::LlmDelta { text } => {
                running.planned.push_str(text.as_str());
                if running.planned.len() > limit {
                    // It has written more than a result may hold. It is
                    // stopped, and what it wrote is the result.
                    self.delegated_done(index, call);
                }
            }
            OpEvent::LlmDone { .. } => {
                running.source = Source::Written;
                self.delegated_done(index, call);
            }
            OpEvent::Failed {
                part: Part::Think, ..
            } => {
                running.source = Source::Written;
                self.call_finished(call, ToolOutcome::Failed, tools::NO_ANSWER);
            }
            // It asks for no tools of its own, and nothing of it is spoken.
            OpEvent::ToolProposed { .. }
            | OpEvent::TtsSegment { .. }
            | OpEvent::Played { .. }
            | OpEvent::Failed { .. } => {}
        }
    }

    /// The agent working on `call` has written its answer.
    fn delegated_done(&mut self, index: usize, call: ToolCallId) {
        let Some(running) = self.state.generations.remove(index) else {
            return;
        };
        if !running.is_written() {
            self.cmd(Command::Llm(running.tag, LlmCmd::Cancel));
        }
        let answer = running.planned.trim();
        if answer.is_empty() {
            self.call_finished(call, ToolOutcome::Failed, tools::NO_ANSWER);
        } else {
            self.call_finished(call, ToolOutcome::Completed, answer);
        }
    }
}
