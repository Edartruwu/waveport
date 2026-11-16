//! What a scenario expects of a session.

use waveport_domain::command::{Command, LlmCause, LlmCmd, PlayoutCmd, ToolCmd, TtsCmd};
use waveport_domain::fact::{Dismissal, Fact, Lifecycle, ToolOutcome};
use waveport_domain::ids::{BatchId, GenId, ParticipantId, ToolCallId, ToolId};
use waveport_domain::kernel::ItemView;

use crate::failure::{Failure, Verdict};
use crate::sim::{Call, Person, Probe, Reply, Seen, Sim, Turn, What};
use crate::world::Ms;

/// How long a session must stay quiet for a scenario to accept that
/// something is not going to happen.
const QUIET_MS: Ms = 3_000;
/// How long a finished scenario is left running, to see what else happens.
const AFTERMATH_MS: Ms = 60_000;

/// How an agent's speech ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// The agent said everything.
    Complete,
    /// The agent was cut off.
    Truncated,
}

/// One item of a conversation, as a scenario writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line(Said);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Said {
    User {
        who: ParticipantId,
        text: String,
    },
    Agent {
        text: String,
        truncated: bool,
    },
    /// Anything a scenario has no way to write: tool calls, results,
    /// notices.
    Other(String),
}

/// `who` said `text`.
#[must_use]
pub fn user(who: Person, text: &str) -> Line {
    Line(Said::User {
        who: who.0,
        text: text.to_owned(),
    })
}

/// The agent said `text`, and all of it.
#[must_use]
pub fn spoke(text: &str) -> Line {
    Line(Said::Agent {
        text: text.to_owned(),
        truncated: false,
    })
}

/// The agent was cut off after `text`.
#[must_use]
pub fn cut_off(text: &str) -> Line {
    Line(Said::Agent {
        text: text.to_owned(),
        truncated: true,
    })
}

impl Line {
    fn of(item: &ItemView<'_>) -> Self {
        Self(match *item {
            ItemView::UserTurn { who, text, .. } => Said::User {
                who,
                text: text.to_owned(),
            },
            ItemView::Utterance {
                text, truncated, ..
            } => Said::Agent {
                text: text.to_owned(),
                truncated,
            },
            other => Said::Other(format!("{other:?}")),
        })
    }
}

/// The response a command steers, if it steers one.
fn generation_of(command: &Command) -> Option<GenId> {
    match command {
        Command::Llm(tag, _) | Command::Tts(tag, _) | Command::Playout(tag, _) => {
            Some(tag.generation)
        }
        _ => None,
    }
}

impl Sim {
    // ---- Looking at what the kernel emitted --------------------------------

    /// The first emission no expectation has used yet that `matches`.
    fn unclaimed(&self, mut matches: impl FnMut(&What) -> bool) -> Option<usize> {
        self.seen
            .iter()
            .position(|seen| !seen.claimed && matches(&seen.what))
    }

    /// Marks an emission as used and returns it.
    fn claim(&mut self, index: usize) -> Option<Seen> {
        let seen = self.seen.get_mut(index)?;
        seen.claimed = true;
        Some(seen.clone())
    }

    fn facts(&self) -> impl Iterator<Item = (&Fact, &str)> {
        self.seen.iter().filter_map(|seen| match &seen.what {
            What::Fact(fact) => Some((fact, seen.text.as_str())),
            What::Cmd(_) => None,
        })
    }

    fn commands(&self) -> impl Iterator<Item = &Command> {
        self.seen.iter().filter_map(|seen| match &seen.what {
            What::Cmd(command) => Some(command),
            What::Fact(_) => None,
        })
    }

    /// The tool a call is for, from the fact that proposed it.
    fn tool_of(&self, call: ToolCallId) -> Option<ToolId> {
        self.facts().find_map(|(fact, _)| match *fact {
            Fact::ToolProposed {
                call: proposed,
                tool,
                ..
            } if proposed == call => Some(tool),
            _ => None,
        })
    }

    /// The batches of tool calls a response proposed.
    fn batches_of(&self, generation: GenId) -> Vec<BatchId> {
        self.facts()
            .filter_map(|(fact, _)| match *fact {
                Fact::ToolProposed {
                    batch,
                    generation: by,
                    ..
                } if by == generation => Some(batch),
                _ => None,
            })
            .collect()
    }

    /// What stopped a response before it had said everything, if anything
    /// did.
    fn interruption_of(&self, generation: GenId) -> Option<&Command> {
        self.commands().find(|command| {
            generation_of(command) == Some(generation)
                && matches!(
                    command,
                    Command::Playout(_, PlayoutCmd::Stop { .. })
                        | Command::Llm(_, LlmCmd::Cancel)
                        | Command::Tts(_, TtsCmd::Cancel)
                )
        })
    }

    /// How many results of `call` the conversation holds.
    fn results_in_context(&self, call: ToolCallId) -> usize {
        self.kernel()
            .view()
            .items()
            .filter(|item| matches!(item, ItemView::ToolResult { call: of, .. } if *of == call))
            .count()
    }

    // ---- Turns -------------------------------------------------------------

    /// A turn by `who` is committed to the conversation, with exactly
    /// these words.
    ///
    /// # Errors
    ///
    /// No turn by `who` is committed, or the next one has other words.
    #[track_caller]
    pub fn expect_turn(&mut self, who: Person, text: &str) -> Result<Turn, Box<Failure>> {
        let name = self.world.names.person(who.0).to_owned();
        let expected = format!("a committed turn in which {name} says \"{text}\"");
        let outcome = self.wait_for(|sim| {
            let found = sim.unclaimed(
                |what| matches!(what, What::Fact(Fact::TurnCommitted { who: by, .. }) if *by == who.0),
            );
            match found.and_then(|index| sim.claim(index)) {
                Some(Seen {
                    what: What::Fact(Fact::TurnCommitted { turn, .. }),
                    text: heard,
                    ..
                }) if heard == text => Probe::Found(Turn(turn)),
                Some(seen) => Probe::Wrong(format!(
                    "the turn that was committed for {name} says \"{}\"",
                    seen.text
                )),
                None => Probe::NotYet,
            }
        });
        self.settle(expected, outcome)
    }

    /// A turn by `who` ends without entering the conversation, for this
    /// reason.
    ///
    /// # Errors
    ///
    /// No turn by `who` is dismissed, or the next one is for another
    /// reason.
    #[track_caller]
    pub fn expect_dismissed(&mut self, who: Person, reason: Dismissal) -> Verdict {
        let name = self.world.names.person(who.0).to_owned();
        let expected = format!("a turn by {name} to be dismissed as {reason:?}");
        let outcome = self.wait_for(|sim| {
            let found = sim.unclaimed(
                |what| matches!(what, What::Fact(Fact::TurnDismissed { who: by, .. }) if *by == who.0),
            );
            match found.and_then(|index| sim.claim(index)).map(|seen| seen.what) {
                Some(What::Fact(Fact::TurnDismissed { reason: why, .. })) if why == reason => {
                    Probe::Found(())
                }
                Some(what) => Probe::Wrong(format!("the kernel emitted {what:?}")),
                None => Probe::NotYet,
            }
        });
        self.settle(expected, outcome)
    }

    // ---- Responses ---------------------------------------------------------

    /// The agent starts a response to `turn`.
    ///
    /// # Errors
    ///
    /// No response to the turn is started.
    #[track_caller]
    pub fn expect_reply(&mut self, turn: Turn) -> Result<Reply, Box<Failure>> {
        let outcome = self.expect_start(|cause| cause == LlmCause::Reply { turn: turn.0 });
        self.settle(format!("a reply to turn {}", turn.0), outcome)
    }

    /// The agent starts no response to `turn` in the next three seconds.
    ///
    /// # Errors
    ///
    /// A response to the turn is started.
    #[track_caller]
    pub fn expect_no_reply(&mut self, turn: Turn) -> Verdict {
        self.run_for(QUIET_MS);
        let outcome = self.look(|sim| {
            let replied = sim.commands().any(|command| {
                matches!(command, Command::Llm(_, LlmCmd::Start { cause, .. })
                    if *cause == LlmCause::Reply { turn: turn.0 })
            });
            if replied {
                Probe::Wrong("the agent started a reply to it".to_owned())
            } else {
                Probe::Found(())
            }
        });
        self.settle(format!("no reply to turn {}", turn.0), outcome)
    }

    /// The agent starts one response to the results of the tool calls that
    /// `reply` asked for.
    ///
    /// # Errors
    ///
    /// No such response is started.
    #[track_caller]
    pub fn expect_continuation(&mut self, reply: Reply) -> Result<Reply, Box<Failure>> {
        let asked = reply.0.generation;
        let outcome = self.wait_for(|sim| {
            let batches = sim.batches_of(asked);
            sim.probe_start(|cause| {
                matches!(cause, LlmCause::Continuation { batch } if batches.contains(&batch))
            })
        });
        self.settle(
            format!("a continuation after the tool calls of response {asked}"),
            outcome,
        )
    }

    /// A back agent starts to work on `call`.
    ///
    /// # Errors
    ///
    /// No response is started for the call.
    #[track_caller]
    pub fn expect_delegated(&mut self, call: Call) -> Result<Reply, Box<Failure>> {
        let outcome = self.expect_start(|cause| cause == LlmCause::Delegated { call: call.0 });
        self.settle(
            format!("a back agent to start work on call {}", call.0),
            outcome,
        )
    }

    fn expect_start(&mut self, wanted: impl Fn(LlmCause) -> bool) -> Result<Reply, String> {
        self.wait_for(|sim| sim.probe_start(&wanted))
    }

    /// Looks for a response that has started for a cause that is `wanted`.
    fn probe_start(&mut self, wanted: impl Fn(LlmCause) -> bool) -> Probe<Reply> {
        let found = self.unclaimed(|what| {
            matches!(what, What::Cmd(Command::Llm(_, LlmCmd::Start { cause, .. })) if wanted(*cause))
        });
        match found
            .and_then(|index| self.claim(index))
            .map(|seen| seen.what)
        {
            Some(What::Cmd(Command::Llm(tag, _))) => Probe::Found(Reply(tag)),
            _ => Probe::NotYet,
        }
    }

    /// Results of `reply`'s work are still acted on: nothing has ended it.
    ///
    /// # Errors
    ///
    /// The response is no longer live.
    #[track_caller]
    pub fn expect_live(&mut self, reply: Reply) -> Verdict {
        let outcome = self.look(|sim| {
            if sim.kernel().view().is_live(reply.0) {
                Probe::Found(())
            } else {
                Probe::Wrong("the kernel no longer acts on its results".to_owned())
            }
        });
        self.settle(
            format!("response {} to be live", reply.0.generation),
            outcome,
        )
    }

    // ---- Speech ------------------------------------------------------------

    /// Time passes until the listener has heard exactly `count` whole
    /// words of `reply`, and the session knows it.
    ///
    /// # Errors
    ///
    /// The response stops before that.
    #[track_caller]
    pub fn hears_words(&mut self, reply: Reply, count: usize) -> Verdict {
        let generation = reply.0.generation;
        let outcome = self.wait_for(|sim| {
            let now = sim.now;
            let Some(voice) = sim.world.voice_of(generation) else {
                return Probe::NotYet;
            };
            voice.advance(now);
            let heard = voice.heard_words();
            if heard == count {
                return Probe::Found(());
            }
            if heard > count {
                return Probe::Wrong(format!("{heard} words had been heard by then"));
            }
            if voice.is_over(now) {
                return Probe::Wrong(format!("the response ended after {heard} words"));
            }
            if let Some(at) = voice.word_eta(now, count) {
                sim.world.wake(at);
            }
            Probe::NotYet
        });
        if outcome.is_ok() {
            self.flush_playout();
        }
        self.settle(
            format!("the listener to hear {count} words of response {generation}"),
            outcome,
        )
    }

    /// The volume of `reply`'s speech is lowered.
    ///
    /// # Errors
    ///
    /// It is not.
    #[track_caller]
    pub fn expect_ducked(&mut self, reply: Reply) -> Verdict {
        let outcome = self.expect_playout(reply, PlayoutCmd::Duck);
        self.settle(
            format!("the speech of response {} to be ducked", reply.0.generation),
            outcome,
        )
    }

    /// The volume of `reply`'s speech is restored.
    ///
    /// # Errors
    ///
    /// It is not.
    #[track_caller]
    pub fn expect_unducked(&mut self, reply: Reply) -> Verdict {
        let outcome = self.expect_playout(reply, PlayoutCmd::Unduck);
        self.settle(
            format!(
                "the speech of response {} to be un-ducked",
                reply.0.generation
            ),
            outcome,
        )
    }

    fn expect_playout(&mut self, reply: Reply, wanted: PlayoutCmd) -> Result<(), String> {
        self.wait_for(|sim| {
            let found = sim.unclaimed(|what| {
                matches!(what, What::Cmd(Command::Playout(tag, command))
                    if tag.generation == reply.0.generation && *command == wanted)
            });
            match found.and_then(|index| sim.claim(index)) {
                Some(_) => Probe::Found(()),
                None => Probe::NotYet,
            }
        })
    }

    /// `reply` is stopped before it has said everything.
    ///
    /// # Errors
    ///
    /// It is not.
    #[track_caller]
    pub fn expect_interrupted(&mut self, reply: Reply) -> Verdict {
        let generation = reply.0.generation;
        let outcome = self.wait_for(|sim| match sim.interruption_of(generation) {
            Some(_) => Probe::Found(()),
            None => Probe::NotYet,
        });
        self.settle(format!("response {generation} to be interrupted"), outcome)
    }

    /// Nothing has stopped `reply` so far.
    ///
    /// # Errors
    ///
    /// Something has.
    #[track_caller]
    pub fn expect_not_interrupted(&mut self, reply: Reply) -> Verdict {
        let generation = reply.0.generation;
        let outcome = self.look(|sim| match sim.interruption_of(generation) {
            Some(command) => Probe::Wrong(format!("the kernel emitted {command:?}")),
            None => Probe::Found(()),
        });
        self.settle(
            format!("response {generation} not to have been interrupted"),
            outcome,
        )
    }

    /// The speech of `reply` enters the conversation as exactly `text`.
    ///
    /// # Errors
    ///
    /// It does not, or with other words, or ending the other way.
    #[track_caller]
    pub fn expect_spoken(&mut self, reply: Reply, text: &str, ending: Ending) -> Verdict {
        let generation = reply.0.generation;
        let expected =
            format!("response {generation} to enter the conversation as \"{text}\", {ending:?}");
        let outcome = self.wait_for(|sim| {
            let found = sim.unclaimed(|what| {
                matches!(what, What::Fact(Fact::UtteranceCommitted { generation: of, .. })
                    if *of == generation)
            });
            let Some(seen) = found.and_then(|index| sim.claim(index)) else {
                return Probe::NotYet;
            };
            let truncated = matches!(
                seen.what,
                What::Fact(Fact::UtteranceCommitted {
                    truncated: true,
                    ..
                })
            );
            let ended = if truncated {
                Ending::Truncated
            } else {
                Ending::Complete
            };
            if seen.text == text && ended == ending {
                Probe::Found(())
            } else {
                Probe::Wrong(format!("it entered as \"{}\", {ended:?}", seen.text))
            }
        });
        self.settle(expected, outcome)
    }

    /// `reply` has put nothing in the conversation and has played nothing.
    ///
    /// # Errors
    ///
    /// It has.
    #[track_caller]
    pub fn expect_never_spoke(&mut self, reply: Reply) -> Verdict {
        let generation = reply.0.generation;
        let outcome = self.look(|sim| {
            let committed = sim.facts().find(|(fact, _)| {
                matches!(fact, Fact::UtteranceCommitted { generation: of, .. } if *of == generation)
            });
            let played = sim.commands().any(|command| {
                matches!(command, Command::Playout(tag, PlayoutCmd::Start) if tag.generation == generation)
            });
            match committed {
                Some((_, text)) => Probe::Wrong(format!("\"{text}\" entered the conversation")),
                None if played => Probe::Wrong("its playback was started".to_owned()),
                None => Probe::Found(()),
            }
        });
        self.settle(
            format!("response {generation} never to have spoken"),
            outcome,
        )
    }

    // ---- Tools -------------------------------------------------------------

    /// A call to the tool named `tool` is dispatched.
    ///
    /// # Errors
    ///
    /// None is.
    #[track_caller]
    pub fn expect_dispatched(&mut self, tool: &str) -> Result<Call, Box<Failure>> {
        let wanted = self.world.names.tool_id(tool);
        if wanted.is_none() {
            self.world.blame(format!(
                "the scenario expects a call to `{tool}`, which it did not set up"
            ));
        }
        let outcome = self.wait_for(|sim| {
            let calls = sim
                .seen
                .iter()
                .enumerate()
                .filter(|(_, seen)| !seen.claimed)
                .filter_map(|(index, seen)| match seen.what {
                    What::Fact(Fact::ToolDispatched { call, .. }) => Some((index, call)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let found = calls
                .into_iter()
                .find(|(_, call)| sim.tool_of(*call) == wanted);
            match found {
                Some((index, call)) => {
                    let _ = sim.claim(index);
                    Probe::Found(Call(call))
                }
                None => Probe::NotYet,
            }
        });
        self.settle(format!("a call to `{tool}` to be dispatched"), outcome)
    }

    /// `call` has not been cancelled, and must not be for the rest of the
    /// scenario.
    ///
    /// # Errors
    ///
    /// It has been.
    #[track_caller]
    pub fn expect_not_cancelled(&mut self, call: Call) -> Verdict {
        self.protected.push(call);
        let outcome = self.look(|sim| {
            if sim.was_cancelled(call.0) {
                Probe::Wrong("the kernel cancelled it".to_owned())
            } else {
                Probe::Found(())
            }
        });
        self.settle(format!("call {} not to be cancelled", call.0), outcome)
    }

    fn was_cancelled(&self, call: ToolCallId) -> bool {
        self.commands()
            .any(|command| *command == Command::Tool(call, ToolCmd::Cancel))
    }

    /// `call` ends with this result, and the conversation holds the result
    /// once.
    ///
    /// # Errors
    ///
    /// The call does not end, ends another way, or its result is in the
    /// conversation more or less than once.
    #[track_caller]
    pub fn expect_result_once(&mut self, call: Call, result: &str) -> Verdict {
        let expected = format!(
            "call {} to end with \"{result}\", once in the conversation",
            call.0
        );
        let outcome = self.wait_for(|sim| {
            let found = sim.unclaimed(
                |what| matches!(what, What::Fact(Fact::ToolFinished { call: of, .. }) if *of == call.0),
            );
            let Some(seen) = found.and_then(|index| sim.claim(index)) else {
                return Probe::NotYet;
            };
            let completed = matches!(
                seen.what,
                What::Fact(Fact::ToolFinished {
                    outcome: ToolOutcome::Completed,
                    ..
                })
            );
            let in_context = sim.results_in_context(call.0);
            if !completed || seen.text != result {
                Probe::Wrong(format!("the kernel emitted {:?} \"{}\"", seen.what, seen.text))
            } else if in_context != 1 {
                Probe::Wrong(format!("the conversation holds its result {in_context} times"))
            } else {
                Probe::Found(())
            }
        });
        self.settle(expected, outcome)
    }

    // ---- The conversation and the session's life ---------------------------

    /// The conversation is exactly these items, in this order.
    ///
    /// # Errors
    ///
    /// It is not.
    #[track_caller]
    pub fn expect_context(&mut self, lines: &[Line]) -> Verdict {
        let outcome = self.look(|sim| {
            let context = sim
                .kernel()
                .view()
                .items()
                .map(|item| Line::of(&item))
                .collect::<Vec<_>>();
            if context == lines {
                Probe::Found(())
            } else {
                Probe::Wrong(format!("the conversation is {context:?}"))
            }
        });
        self.settle(format!("the conversation to be {lines:?}"), outcome)
    }

    /// The session moves to this stage of its life.
    ///
    /// # Errors
    ///
    /// It does not.
    #[track_caller]
    pub fn expect_lifecycle(&mut self, stage: Lifecycle) -> Verdict {
        let outcome = self.wait_for(|sim| {
            let found = sim.unclaimed(
                |what| matches!(what, What::Fact(Fact::LifecycleChanged { to, .. }) if *to == stage),
            );
            match found.and_then(|index| sim.claim(index)) {
                Some(_) => Probe::Found(()),
                None => Probe::NotYet,
            }
        });
        self.settle(format!("the session to become {stage:?}"), outcome)
    }

    /// The session is `Closed` no later than `millis` after it was asked
    /// to close.
    ///
    /// # Errors
    ///
    /// It is not, or it was never asked to close.
    #[track_caller]
    pub fn expect_closed_within(&mut self, millis: u64) -> Verdict {
        if let Some(asked) = self.closed_at {
            let deadline = asked.saturating_add(millis);
            self.run_for(deadline.saturating_sub(self.now));
        }
        let outcome = self.look(|sim| {
            let stage = sim.kernel().view().lifecycle();
            match sim.closed_at {
                None => Probe::Wrong("the scenario never closed the session".to_owned()),
                Some(_) if stage == Lifecycle::Closed => Probe::Found(()),
                Some(_) => Probe::Wrong(format!("it is still {stage:?} at that time")),
            }
        });
        self.settle(
            format!("the session to be Closed within {millis} ms of being asked"),
            outcome,
        )
    }

    /// The scenario is over. The session is left running for a minute to
    /// see what else it does, and then everything that must hold of any
    /// session is checked:
    ///
    /// - every scripted response was given, and no other;
    /// - everything a person said ended in one turn, committed or
    ///   dismissed;
    /// - no response entered the conversation twice;
    /// - no tool call was dispatched twice, ended twice or has its result
    ///   in the conversation twice;
    /// - no call the scenario protected was cancelled.
    ///
    /// # Errors
    ///
    /// Any of these does not hold.
    #[track_caller]
    pub fn finish(&mut self) -> Verdict {
        self.run_to_rest(AFTERMATH_MS);
        let mut problems = self.world.unused_scripts();
        if let Some(fault) = self.world.fault() {
            problems.push(fault.to_owned());
        }
        self.check_turns(&mut problems);
        self.check_utterances(&mut problems);
        self.check_calls(&mut problems);
        let outcome = if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        };
        self.settle(
            "the session to end with nothing wrong and nothing left over",
            outcome,
        )
    }

    fn check_turns(&self, problems: &mut Vec<String>) {
        let mut speakers = self
            .facts()
            .filter_map(|(fact, _)| match *fact {
                Fact::TurnCommitted { who, .. } | Fact::TurnDismissed { who, .. } => Some(who),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut everyone = 1;
        while self.world.utterances_by(ParticipantId(everyone)) > 0 {
            speakers.push(ParticipantId(everyone));
            everyone += 1;
        }
        speakers.sort_unstable();
        speakers.dedup();
        for who in speakers {
            let spoke = self.world.utterances_by(who);
            let turns = self
                .facts()
                .filter(|(fact, _)| {
                    matches!(fact, Fact::TurnCommitted { who: by, .. } | Fact::TurnDismissed { who: by, .. }
                        if *by == who)
                })
                .count();
            // Once the adapters are silent, what a person says may never
            // reach the session at all.
            let lost = self.world.is_silent() && turns < spoke;
            if turns != spoke && !lost {
                problems.push(format!(
                    "{} spoke {spoke} times, and {turns} turns of theirs were committed or dismissed",
                    self.world.names.person(who)
                ));
            }
        }
    }

    fn check_utterances(&self, problems: &mut Vec<String>) {
        let mut committed = self
            .facts()
            .filter_map(|(fact, _)| match *fact {
                Fact::UtteranceCommitted { generation, .. } => Some(generation),
                _ => None,
            })
            .collect::<Vec<_>>();
        committed.sort_unstable();
        for pair in committed.windows(2) {
            if let [first, second] = pair
                && first == second
            {
                problems.push(format!("response {first} entered the conversation twice"));
            }
        }
    }

    fn check_calls(&self, problems: &mut Vec<String>) {
        let mut calls = self
            .facts()
            .filter_map(|(fact, _)| match *fact {
                Fact::ToolProposed { call, .. } => Some(call),
                _ => None,
            })
            .collect::<Vec<_>>();
        calls.sort_unstable();
        calls.dedup();
        for call in calls {
            let name = self
                .tool_of(call)
                .map_or("an unknown tool", |tool| self.world.names.tool(tool));
            let dispatched = self
                .facts()
                .filter(|(fact, _)| {
                    matches!(fact, Fact::ToolDispatched { call: of, attempt: 0 } if *of == call)
                })
                .count();
            let sent = self
                .commands()
                .filter(|command| {
                    matches!(command, Command::Tool(of, ToolCmd::Dispatch { attempt: 0, .. }) if *of == call)
                })
                .count();
            let finished = self
                .facts()
                .filter(
                    |(fact, _)| matches!(fact, Fact::ToolFinished { call: of, .. } if *of == call),
                )
                .count();
            let in_context = self.results_in_context(call);
            if dispatched > 1 || sent > 1 {
                problems.push(format!(
                    "call {call} to `{name}` was dispatched more than once"
                ));
            }
            if finished > 1 || in_context > 1 {
                problems.push(format!(
                    "call {call} to `{name}` ended {finished} times and has {in_context} results in the conversation"
                ));
            }
        }
        for call in &self.protected {
            if self.was_cancelled(call.0) {
                problems.push(format!("call {} was cancelled", call.0));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! The kernel these tests run against does nothing yet, so what it
    //! would emit is written into the session's record by hand.

    use std::rc::Rc;

    use waveport_domain::command::{Command, LlmCause, LlmCmd, PlayoutCmd, ToolCmd, TtsCmd};
    use waveport_domain::config::Effect;
    use waveport_domain::fact::{Dismissal, Fact, Lifecycle, ToolOutcome};
    use waveport_domain::ids::{
        AgentId, BatchId, ContextVersion, Epoch, GenId, OpTag, ParticipantId, ToolCallId, ToolId,
        TurnId,
    };
    use waveport_domain::text::{TextArena, TextRef};
    use waveport_domain::time::MediaSpan;

    use super::{Ending, cut_off, spoke, user};
    use crate::explore::{Explored, explore};
    use crate::script::say;
    use crate::setup::Setup;
    use crate::sim::{Call, Reply, Seen, Sim, Turn, What};
    use crate::world::Timer;

    const CALLER: ParticipantId = ParticipantId(1);

    fn tag(generation: u32) -> OpTag {
        OpTag {
            generation: GenId(generation),
            epoch: Epoch(0),
        }
    }

    /// A session whose kernel emits nothing, so that each test decides
    /// what was emitted and the expectations are tested on their own.
    fn session(setup: Setup) -> Sim {
        let mut sim = live(setup);
        sim.silence_kernel();
        sim
    }

    /// A session with a working kernel.
    fn live(setup: Setup) -> Sim {
        Sim::new(setup, Rc::default())
    }

    fn with_tools() -> Sim {
        session(
            Setup::new()
                .tool("lookup", Effect::Pure)
                .tool("refund", Effect::SideEffecting),
        )
    }

    /// Records `what` as if the kernel had emitted it.
    fn emit(sim: &mut Sim, what: What, text: &str) {
        sim.seen.push(Seen {
            what,
            text: text.to_owned(),
            claimed: false,
        });
    }

    fn fact(sim: &mut Sim, fact: Fact, text: &str) {
        emit(sim, What::Fact(fact), text);
    }

    fn command(sim: &mut Sim, command: Command) {
        emit(sim, What::Cmd(command), "");
    }

    fn committed(sim: &mut Sim, turn: u32, who: ParticipantId, text: &str) {
        let fact_ = Fact::TurnCommitted {
            turn: TurnId(turn),
            who,
            span: MediaSpan::default(),
            text: TextRef::EMPTY,
            addressed: true,
        };
        fact(sim, fact_, text);
    }

    fn started(sim: &mut Sim, generation: u32, agent: u8, cause: LlmCause) {
        let start = LlmCmd::Start {
            agent: AgentId(agent),
            context: ContextVersion(1),
            cause,
        };
        command(sim, Command::Llm(tag(generation), start));
    }

    fn spoken(sim: &mut Sim, generation: u32, text: &str, truncated: bool) {
        let fact_ = Fact::UtteranceCommitted {
            generation: GenId(generation),
            agent: AgentId(0),
            text: TextRef::EMPTY,
            truncated,
        };
        fact(sim, fact_, text);
    }

    fn proposed(sim: &mut Sim, call: u64, tool: u16, generation: u32) {
        let fact_ = Fact::ToolProposed {
            call: ToolCallId(call),
            batch: BatchId(1),
            generation: GenId(generation),
            tool: ToolId(tool),
            args: TextRef::EMPTY,
        };
        fact(sim, fact_, "{}");
    }

    fn dispatched(sim: &mut Sim, call: u64) {
        let call = ToolCallId(call);
        fact(sim, Fact::ToolDispatched { call, attempt: 0 }, "");
    }

    #[test]
    fn a_turn_is_matched_by_speaker_and_words_and_only_once() {
        let mut sim = session(Setup::new());
        let caller = sim.caller();
        let ben = sim.joins("Ben");
        committed(&mut sim, 1, ben.0, "yes i sent it");
        committed(&mut sim, 2, CALLER, "where is my order");

        assert_eq!(
            sim.expect_turn(caller, "where is my order"),
            Ok(Turn(TurnId(2)))
        );
        assert_eq!(sim.expect_turn(ben, "yes i sent it"), Ok(Turn(TurnId(1))));
        // Both are used up: a second turn with the same words is another turn.
        let failure = sim.expect_turn(caller, "where is my order").unwrap_err();
        assert_eq!(failure.ordinal, 3);
        assert!(failure.found.contains("never happened"), "{failure}");
        assert!(failure.expected.contains("the caller says"), "{failure}");
    }

    #[test]
    fn a_turn_with_other_words_fails_at_once() {
        let mut sim = session(Setup::new());
        let caller = sim.caller();
        committed(&mut sim, 1, CALLER, "where is my border");
        let failure = sim.expect_turn(caller, "where is my order").unwrap_err();
        assert!(failure.found.contains("where is my border"), "{failure}");
        assert_eq!(failure.file, file!());
    }

    #[test]
    fn a_dismissal_is_matched_by_its_reason() {
        let mut sim = session(Setup::new());
        let caller = sim.caller();
        let dismissed = |reason| Fact::TurnDismissed {
            turn: TurnId(1),
            who: CALLER,
            reason,
        };
        fact(&mut sim, dismissed(Dismissal::Backchannel), "");
        fact(&mut sim, dismissed(Dismissal::Noise), "");
        assert_eq!(sim.expect_dismissed(caller, Dismissal::Backchannel), Ok(()));
        let failure = sim.expect_dismissed(caller, Dismissal::Closed).unwrap_err();
        assert!(failure.found.contains("Noise"), "{failure}");
    }

    #[test]
    fn responses_are_told_apart_by_what_they_are_for() {
        let mut sim = with_tools();
        proposed(&mut sim, 7, 0, 1);
        started(&mut sim, 1, 0, LlmCause::Reply { turn: TurnId(1) });
        started(
            &mut sim,
            2,
            1,
            LlmCause::Delegated {
                call: ToolCallId(7),
            },
        );
        started(&mut sim, 3, 0, LlmCause::Continuation { batch: BatchId(1) });

        let reply = sim.expect_reply(Turn(TurnId(1))).unwrap();
        assert_eq!(reply, Reply(tag(1)));
        assert_eq!(sim.expect_delegated(Call(ToolCallId(7))), Ok(Reply(tag(2))));
        assert_eq!(sim.expect_continuation(reply), Ok(Reply(tag(3))));
        // One of each, and each is used up.
        assert!(sim.expect_continuation(reply).is_err());
        assert!(sim.expect_reply(Turn(TurnId(1))).is_err());
        // A continuation of another response's calls is not this one's.
        started(&mut sim, 4, 0, LlmCause::Continuation { batch: BatchId(9) });
        assert!(sim.expect_continuation(reply).is_err());
    }

    #[test]
    fn no_reply_means_none_in_three_seconds() {
        let mut sim = session(Setup::new());
        assert_eq!(sim.expect_no_reply(Turn(TurnId(1))), Ok(()));
        assert_eq!(sim.now, 3_000);
        started(&mut sim, 1, 0, LlmCause::Reply { turn: TurnId(1) });
        assert_eq!(sim.expect_no_reply(Turn(TurnId(2))), Ok(()));
        let failure = sim.expect_no_reply(Turn(TurnId(1))).unwrap_err();
        assert!(failure.found.contains("started a reply"), "{failure}");
    }

    #[test]
    fn ducking_and_interruption_are_read_from_the_commands() {
        let mut sim = session(Setup::new());
        let reply = Reply(tag(1));
        let other = Reply(tag(2));
        command(&mut sim, Command::Playout(tag(1), PlayoutCmd::Duck));
        command(&mut sim, Command::Playout(tag(1), PlayoutCmd::Unduck));
        assert_eq!(sim.expect_ducked(reply), Ok(()));
        assert_eq!(sim.expect_unducked(reply), Ok(()));
        assert!(sim.expect_ducked(reply).is_err(), "it was ducked once");
        assert!(sim.expect_ducked(other).is_err());
        assert_eq!(sim.expect_not_interrupted(reply), Ok(()));
        assert!(sim.expect_interrupted(reply).is_err());

        command(
            &mut sim,
            Command::Playout(tag(1), PlayoutCmd::Stop { fade_ms: 8 }),
        );
        assert_eq!(sim.expect_interrupted(reply), Ok(()));
        let failure = sim.expect_not_interrupted(reply).unwrap_err();
        assert!(failure.found.contains("Stop"), "{failure}");
        // A response that never played is interrupted by cancelling it.
        command(&mut sim, Command::Llm(tag(2), LlmCmd::Cancel));
        assert_eq!(sim.expect_interrupted(other), Ok(()));
        command(&mut sim, Command::Tts(tag(3), TtsCmd::Cancel));
        assert_eq!(sim.expect_interrupted(Reply(tag(3))), Ok(()));
    }

    #[test]
    fn spoken_text_must_match_in_words_and_in_ending() {
        let mut sim = session(Setup::new());
        spoken(&mut sim, 1, "Your order shipped", true);
        spoken(&mut sim, 2, "Done.", false);
        spoken(&mut sim, 3, "Done.", true);
        assert_eq!(
            sim.expect_spoken(Reply(tag(1)), "Your order shipped", Ending::Truncated),
            Ok(())
        );
        assert_eq!(
            sim.expect_spoken(Reply(tag(2)), "Done.", Ending::Complete),
            Ok(())
        );
        let failure = sim
            .expect_spoken(Reply(tag(3)), "Done.", Ending::Complete)
            .unwrap_err();
        assert!(failure.found.contains("\"Done.\", Truncated"), "{failure}");

        assert_eq!(sim.expect_never_spoke(Reply(tag(4))), Ok(()));
        assert!(sim.expect_never_spoke(Reply(tag(1))).is_err());
        command(&mut sim, Command::Playout(tag(4), PlayoutCmd::Start));
        let failure = sim.expect_never_spoke(Reply(tag(4))).unwrap_err();
        assert!(failure.found.contains("playback"), "{failure}");
    }

    #[test]
    fn hearing_words_stops_the_clock_at_the_word_and_tells_the_kernel() {
        let mut sim = session(Setup::new());
        let reply = Reply(tag(1));
        let mut arena = TextArena::default();
        let text = arena.push("Your order shipped on Monday.");
        let now = sim.now;
        let speak = Command::Tts(tag(1), TtsCmd::Speak { text });
        sim.world.on_command(now, &speak, &arena);
        let start = Command::Playout(tag(1), PlayoutCmd::Start);
        sim.world.on_command(now, &start, &arena);

        // Audio arrives after 100 ms, and three words take 900 ms.
        assert_eq!(sim.hears_words(reply, 3), Ok(()));
        assert_eq!(sim.now, 1_000);
        assert_eq!(
            sim.world
                .voice_of(GenId(1))
                .map(|voice| voice.heard_text().to_owned()),
            Some("Your order shipped".to_owned())
        );
        let failure = sim.hears_words(reply, 2).unwrap_err();
        assert!(
            failure.found.contains("3 words had been heard"),
            "{failure}"
        );
        // The position at that moment was reported to the kernel.
        assert!(
            failure
                .log
                .iter()
                .any(|line| line.contains("1000 ms") && line.contains("Played")),
            "{failure}"
        );

        let stop = Command::Playout(tag(1), PlayoutCmd::Stop { fade_ms: 0 });
        sim.world.on_command(1_000, &stop, &arena);
        let failure = sim.hears_words(reply, 5).unwrap_err();
        assert!(failure.found.contains("ended after 3 words"), "{failure}");
    }

    #[test]
    fn calls_are_found_by_tool_name() {
        let mut sim = with_tools();
        proposed(&mut sim, 7, 1, 1);
        proposed(&mut sim, 8, 0, 1);
        dispatched(&mut sim, 7);
        dispatched(&mut sim, 8);
        let lookup = sim.expect_dispatched("lookup").unwrap();
        let refund = sim.expect_dispatched("refund").unwrap();
        assert_eq!((lookup, refund), (Call(ToolCallId(8)), Call(ToolCallId(7))));
        assert!(
            sim.expect_dispatched("refund").is_err(),
            "it was dispatched once"
        );

        assert_eq!(sim.expect_not_cancelled(refund), Ok(()));
        command(&mut sim, Command::Tool(ToolCallId(7), ToolCmd::Cancel));
        assert!(sim.expect_not_cancelled(refund).is_err());
        assert_eq!(sim.expect_not_cancelled(lookup), Ok(()));
        let failure = sim.finish().unwrap_err();
        assert!(failure.found.contains("call 7 was cancelled"), "{failure}");
    }

    #[test]
    fn an_unknown_tool_is_the_scenarios_mistake() {
        let mut sim = session(Setup::new());
        let failure = sim.expect_dispatched("teleport").unwrap_err();
        assert!(failure.found.contains("did not set up"), "{failure}");
    }

    #[test]
    fn a_result_must_also_be_in_the_conversation() {
        let mut sim = with_tools();
        let finished = Fact::ToolFinished {
            call: ToolCallId(7),
            outcome: ToolOutcome::Completed,
            result: TextRef::EMPTY,
        };
        fact(&mut sim, finished, "{\"ok\":true}");
        // The kernel under test keeps no conversation yet.
        let failure = sim
            .expect_result_once(Call(ToolCallId(7)), "{\"ok\":true}")
            .unwrap_err();
        assert!(failure.found.contains("0 times"), "{failure}");

        fact(&mut sim, finished, "{\"ok\":false}");
        let failure = sim
            .expect_result_once(Call(ToolCallId(7)), "{\"ok\":true}")
            .unwrap_err();
        assert!(
            failure.found.contains("ok\\\":false") || failure.found.contains("false"),
            "{failure}"
        );
    }

    #[test]
    fn the_conversation_is_compared_as_a_whole() {
        let mut sim = session(Setup::new());
        let caller = sim.caller();
        assert_eq!(sim.expect_context(&[]), Ok(()));
        let failure = sim
            .expect_context(&[user(caller, "hello"), cut_off("Hi"), spoke("Hi there.")])
            .unwrap_err();
        assert!(
            failure.found.contains("the conversation is []"),
            "{failure}"
        );
        assert!(failure.expected.contains("truncated: true"), "{failure}");
    }

    #[test]
    fn the_stages_of_a_sessions_life_are_read_from_facts_and_from_the_kernel() {
        let mut sim = session(Setup::new());
        let failure = sim.expect_closed_within(1_000).unwrap_err();
        assert!(failure.found.contains("never closed"), "{failure}");

        sim.wait_ms(500);
        sim.close();
        let draining = Fact::LifecycleChanged {
            to: Lifecycle::Draining,
            reason: None,
        };
        fact(&mut sim, draining, "");
        assert_eq!(sim.expect_lifecycle(Lifecycle::Draining), Ok(()));
        assert!(sim.expect_lifecycle(Lifecycle::Closed).is_err());
        let failure = sim.expect_closed_within(3_000).unwrap_err();
        assert!(failure.found.contains("still Running"), "{failure}");
        assert_eq!(failure.at_ms, 3_500);
        assert_eq!(
            sim.expect_live(Reply(tag(1)))
                .map_err(|failure| failure.ordinal),
            Err(5)
        );
    }

    #[test]
    fn finishing_checks_what_must_hold_of_any_session() {
        // Nothing happened: nothing is wrong.
        let mut sim = session(Setup::new());
        assert_eq!(sim.finish(), Ok(()));

        // Words that end in no turn, and a response never given.
        let mut sim = with_tools();
        let caller = sim.caller();
        sim.agent_will(say("Hello."));
        sim.says(caller, "hello there");
        // Twice into the conversation, twice dispatched, twice ended.
        spoken(&mut sim, 1, "Hello.", false);
        spoken(&mut sim, 1, "Hello.", false);
        proposed(&mut sim, 7, 1, 1);
        dispatched(&mut sim, 7);
        dispatched(&mut sim, 7);
        let finished = Fact::ToolFinished {
            call: ToolCallId(7),
            outcome: ToolOutcome::Completed,
            result: TextRef::EMPTY,
        };
        fact(&mut sim, finished, "{}");
        fact(&mut sim, finished, "{}");
        let failure = sim.finish().unwrap_err();
        for problem in [
            "the agent never gave the response \"Hello.\"",
            "the caller spoke 1 times, and 0 turns of theirs",
            "response 1 entered the conversation twice",
            "call 7 to `refund` was dispatched more than once",
            "call 7 to `refund` ended 2 times",
        ] {
            assert!(failure.found.contains(problem), "{problem}: {failure}");
        }
        // Everything the caller's speech sent has arrived by now.
        assert_eq!(sim.now, 800);
    }

    #[test]
    fn words_lost_to_silent_adapters_are_not_held_against_the_session() {
        let mut sim = session(Setup::new());
        let caller = sim.caller();
        sim.says(caller, "hello there");
        sim.lose_timer(Timer::Drain);
        sim.adapters_go_silent();
        sim.wait_ms(1_000);
        sim.says(caller, "anyone");
        sim.close();
        assert_eq!(sim.finish(), Ok(()));
    }

    #[test]
    fn a_broken_setup_fails_the_first_check() {
        let mut sim = session(Setup::new().delegate("research", "nobody"));
        let caller = sim.caller();
        let failure = sim.expect_turn(caller, "hello").unwrap_err();
        assert_eq!(failure.ordinal, 1);
        assert!(failure.found.contains("not a back agent"), "{failure}");

        let mut sim = session(Setup::new());
        sim.back_agent_will("researcher", say("Nothing."));
        let failure = sim.finish().unwrap_err();
        assert!(
            failure.found.contains("not an agent of the setup"),
            "{failure}"
        );
    }

    #[test]
    fn a_working_kernel_closes_in_stages() {
        let mut sim = live(Setup::new());
        let caller = sim.caller();
        // Thirty words: nine seconds of speech.
        let long = "one two three four five six seven eight nine ten ".repeat(3);
        sim.agent_will(say(long.trim_end()));
        sim.says(caller, "hello");
        let turn = sim.expect_turn(caller, "hello").unwrap();
        let reply = sim.expect_reply(turn).unwrap();
        sim.hears_words(reply, 2).unwrap();

        // Asked to close in the middle of the answer, the session lets the
        // agent go on for the five seconds it has to finish.
        sim.close();
        let asked = sim.now;
        assert_eq!(sim.expect_lifecycle(Lifecycle::Draining), Ok(()));
        let failure = sim.expect_closed_within(3_000).unwrap_err();
        assert!(failure.found.contains("still Draining"), "{failure}");
        assert_eq!((failure.ordinal, failure.at_ms), (5, asked + 3_000));
        assert_eq!(sim.expect_closed_within(5_000), Ok(()));
        assert_eq!(sim.now, asked + 5_000);
        assert_eq!(sim.expect_lifecycle(Lifecycle::Closed), Ok(()));
        assert_eq!(sim.expect_interrupted(reply), Ok(()));
        assert_eq!(sim.finish(), Ok(()));
    }

    #[test]
    fn a_race_runs_the_scenario_once_for_every_order() {
        let explored = explore(|lab| {
            let mut sim = lab.sim(Setup::new());
            let caller = sim.caller();
            sim.agent_will(say("Hello."));
            // Four reports are on their way: an interim and a final
            // transcript, in that order, and the end of the speech followed
            // by the estimate that it asks for.
            sim.says(caller, "hello there");
            sim.race()?;
            sim.expect_turn(caller, "hello there")?;
            sim.finish()
        });
        assert_eq!(explored, Ok(Explored { paths: 6 }));
    }

    #[test]
    fn a_failure_names_the_path_and_shows_the_last_steps() {
        let failure = explore(|lab| {
            let mut sim = lab.sim(Setup::new());
            let caller = sim.caller();
            let late = lab.either();
            sim.says(caller, "hello");
            if late {
                sim.wait_ms(50);
                sim.expect_turn(caller, "hello")?;
            }
            Ok(())
        })
        .unwrap_err();
        assert_eq!(failure.path, [1]);
        assert_eq!(failure.ordinal, 1);
        let shown = failure.to_string();
        assert!(shown.contains("check 1 failed at"), "{shown}");
        assert!(shown.contains("path:     [1]"), "{shown}");
        assert!(shown.contains("<- Speech"), "{shown}");
        assert!(shown.contains("is_final: true"), "{shown}");
    }
}
