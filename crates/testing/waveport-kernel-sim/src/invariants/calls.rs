//! The monitor's account of tool calls, their batches, and the floor
//! changing hands.
//!
//! A call is followed from the model's request to its one result: that it
//! is cleared only when its gate is open, sent only once the record that
//! cleared it is stored if it may change anything, never sent for a
//! response that was cut off, sent once in one life of the session and
//! again in the next only if that does no harm, and that it ends in a way
//! the events account for.

use waveport_domain::command::{LlmCause, ToolCmd};
use waveport_domain::config::{Effect, Executor, Gate, ToolSpec};
use waveport_domain::event::{ControlInput, Event, HandoffContext, OpEvent, ToolEvent};
use waveport_domain::fact::{Lifecycle, NoticeKind, ToolOutcome};
use waveport_domain::ids::{AgentId, BatchId, GenId, OpTag, ToolCallId, ToolId};
use waveport_domain::kernel::{CallState, ItemView, View};

use super::{Monitor, Rule, Violation, ensure};

/// A tool call that has not ended, as the monitor has it.
#[derive(Clone, Debug, Hash)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "four things that are each true or not of a call, in any combination"
)]
pub(super) struct Asked {
    call: ToolCallId,
    tool: ToolId,
    batch: BatchId,
    by: GenId,
    args: String,
    /// The fact that cleared it, if it is cleared; 0 when that was in an
    /// earlier life of the session.
    cleared: Option<u64>,
    /// Whether the journal has it as sent, in this life or an earlier one.
    dispatched: bool,
    attempt: u8,
    /// Whether it was sent in this life of the session.
    sent: bool,
    /// Whether it was asked for in an earlier life of the session.
    inherited: bool,
    /// Whether its tool was told to stop.
    stopped: bool,
}

/// A batch that is not resolved, as the monitor has it.
#[derive(Clone, Copy, Debug, Hash)]
pub(super) struct Asking {
    batch: BatchId,
    by: GenId,
    pending: usize,
    /// Whether any of its calls was sent, or ended in another way than by
    /// being withdrawn.
    ran: bool,
    /// Whether the response that asked has ended.
    over: bool,
    /// Whether it ended by being cut off.
    cut: bool,
}

/// What the first step of a session that was rebuilt has to do.
#[derive(Clone, Debug, Hash)]
pub(super) struct Recovery {
    /// The speech that was being heard when the session stopped, as far
    /// as the journal has it.
    pub(super) speech: Option<(GenId, String)>,
    /// Whether anything was under way that is now lost.
    pub(super) lost: bool,
    pub(super) noted: bool,
}

/// The monitor's account of tools and the stage.
#[derive(Clone, Debug, Default, Hash)]
pub(super) struct Desk {
    pub(super) calls: Vec<Asked>,
    pub(super) batches: Vec<Asking>,
    /// The batches a continuation was started for.
    continued: Vec<BatchId>,
    last_call: u64,
    last_batch: u32,
    /// Every fact up to this one is stored.
    pub(super) durable: u64,
    pub(super) front: AgentId,
    pub(super) handoff: Option<AgentId>,
    /// What the response that is owed is for.
    pub(super) cause: Option<LlmCause>,
    pub(super) recovery: Option<Recovery>,

    // What is known of the step being looked at.
    /// The responses that hold the floor and end in it, and whether each
    /// is cut off.
    pub(super) ending: Vec<(GenId, bool)>,
    /// The calls the facts say were sent, and nothing has been seen to
    /// send yet.
    sending: Vec<(ToolCallId, u8)>,
    /// The records whose storing has to be asked for.
    awaiting: Vec<u64>,
    /// The calls that ended.
    ended: Vec<Asked>,
    /// The language models told to stop.
    pub(super) stopping: Vec<OpTag>,
    /// The responses that worked on a call that ended while their
    /// language model was writing, and that nothing has been seen to stop
    /// yet.
    retired: Vec<OpTag>,
    /// Whether this is the first step of a session that was rebuilt.
    pub(super) recovering: bool,
}

impl Desk {
    /// The account of a session in the state `view` shows.
    pub(super) fn of(view: View<'_>) -> Self {
        let args = |call: ToolCallId| {
            view.items()
                .find_map(|item| match item {
                    ItemView::ToolCall { call: of, args, .. } if of == call => Some(args),
                    _ => None,
                })
                .unwrap_or("")
                .to_owned()
        };
        let calls = view
            .calls()
            .map(|open| Asked {
                call: open.call,
                tool: open.tool,
                batch: open.batch,
                by: open.by,
                args: args(open.call),
                cleared: (open.state != CallState::Proposed).then_some(0),
                dispatched: matches!(open.state, CallState::Dispatched | CallState::Cancelling),
                attempt: open.attempt,
                sent: false,
                inherited: true,
                stopped: false,
            })
            .collect::<Vec<_>>();
        let batches = view
            .batches()
            .map(|open| Asking {
                batch: open.batch,
                by: open.by,
                pending: open.pending,
                ran: open.ran,
                // Whoever asked is gone with the session's earlier life.
                over: true,
                cut: false,
            })
            .collect::<Vec<_>>();
        let last_call = view
            .items()
            .filter_map(|item| match item {
                ItemView::ToolCall { call, .. } => Some(call.get()),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        Self {
            last_batch: batches
                .iter()
                .map(|open| open.batch.get())
                .max()
                .unwrap_or(0),
            calls,
            batches,
            last_call,
            durable: view.next_seq().get().saturating_sub(1),
            front: view.front(),
            handoff: view.handoff(),
            cause: view.owed(),
            ..Self::default()
        }
    }
}

fn broke(rule: Rule, what: String) -> Violation {
    Violation { rule, what }
}

impl Monitor {
    fn spec(&self, tool: ToolId) -> Option<ToolSpec> {
        self.config.tool(tool).copied()
    }

    /// Whether the response `by` is being cut off in this step, or was
    /// before.
    fn is_cut(&self, batch: BatchId, by: GenId) -> bool {
        let cut = self
            .desk
            .batches
            .iter()
            .any(|open| open.batch == batch && open.cut);
        cut || self.desk.ending.contains(&(by, true))
    }

    /// The responses that are cut off in this step are cut off from here
    /// on in its facts.
    pub(super) fn floor_cut(&mut self) {
        for index in 0..self.desk.ending.len() {
            let Some(&(generation, true)) = self.desk.ending.get(index) else {
                continue;
            };
            for open in &mut self.desk.batches {
                if open.by == generation {
                    open.over = true;
                    open.cut = true;
                }
            }
        }
    }

    /// The response `generation` ends in this step without being cut off.
    pub(super) fn asker_done(&mut self, generation: GenId) {
        for open in &mut self.desk.batches {
            if open.by == generation {
                open.over = true;
            }
        }
    }

    pub(super) fn call_proposed(
        &mut self,
        event: &Event,
        call: ToolCallId,
        batch: BatchId,
        generation: GenId,
        tool: ToolId,
        args: &str,
    ) -> Result<(), Violation> {
        self.context += 1;
        let asked = matches!(event, Event::Op(tag, OpEvent::ToolProposed { tool: wanted, args: with })
            if tag.generation == generation && *wanted == tool && with.as_str() == args);
        let may = self.responses.iter().any(|spoken| {
            let given = spoken
                .agent
                .and_then(|agent| self.config.agent(agent))
                .is_some_and(|agent| agent.tools.contains(&tool));
            spoken.tag.generation == generation
                && spoken.asked
                && spoken.works_on.is_none()
                && !spoken.done
                && given
        });
        ensure(asked && may, Rule::Tools, || {
            format!(
                "call {call} to tool {tool} with {args:?} was recorded for response {generation}, \
                 which did not or may not ask for it; the event is {event:?}"
            )
        })?;
        let fresh = call.get() > self.desk.last_call;
        ensure(fresh, Rule::Tools, || {
            format!(
                "call {call} was asked for after call {}",
                self.desk.last_call
            )
        })?;
        self.desk.last_call = call.get();

        let room = self.desk.calls.len() < self.config.limits.open_calls;
        ensure(room, Rule::Bounds, || {
            format!("more than {} calls are open", self.config.limits.open_calls)
        })?;
        let last_batch = self.desk.last_batch;
        let asking = self
            .desk
            .batches
            .iter_mut()
            .find(|open| open.by == generation);
        if let Some(open) = asking {
            ensure(open.batch == batch, Rule::Tools, || {
                format!(
                    "response {generation} asked for call {call} in batch {batch}; its batch is {}",
                    open.batch
                )
            })?;
            open.pending += 1;
        } else {
            ensure(batch.get() > last_batch, Rule::Tools, || {
                format!("batch {batch} began after batch {last_batch}")
            })?;
            self.desk.last_batch = batch.get();
            self.desk.batches.push(Asking {
                batch,
                by: generation,
                pending: 1,
                ran: false,
                over: false,
                cut: false,
            });
        }
        self.desk.calls.push(Asked {
            call,
            tool,
            batch,
            by: generation,
            args: args.to_owned(),
            cleared: None,
            dispatched: false,
            attempt: 0,
            sent: false,
            inherited: false,
            stopped: false,
        });
        Ok(())
    }

    pub(super) fn call_cleared(&mut self, seq: u64, call: ToolCallId) -> Result<(), Violation> {
        let running = self.lifecycle == Lifecycle::Running;
        let Some(asked) = self.desk.calls.iter().find(|asked| asked.call == call) else {
            return Err(broke(
                Rule::Tools,
                format!("call {call} was cleared to run, and it is not open"),
            ));
        };
        let spec = self.spec(asked.tool);
        let asker_over = self
            .desk
            .batches
            .iter()
            .find(|open| open.batch == asked.batch)
            .is_none_or(|open| open.over);
        let gate_open = spec.is_some_and(|spec| spec.gate == Gate::None || asker_over);
        // A response is cut off where the facts say so, and not before.
        let cut = self
            .desk
            .batches
            .iter()
            .any(|open| open.batch == asked.batch && open.cut);
        let holds = asked.cleared.is_none() && running && gate_open && !cut;
        ensure(holds, Rule::Tools, || {
            format!(
                "call {call} was cleared to run; cleared before: {}, the session is {:?}, its \
                 gate is open: {gate_open}, its response was cut off: {cut}",
                asked.cleared.is_some(),
                self.lifecycle
            )
        })?;
        if spec.is_some_and(|spec| spec.effect != Effect::Pure) {
            self.desk.awaiting.push(seq);
        }
        if let Some(asked) = self.desk.calls.iter_mut().find(|asked| asked.call == call) {
            asked.cleared = Some(seq);
        }
        Ok(())
    }

    pub(super) fn call_sent(&mut self, call: ToolCallId, attempt: u8) -> Result<(), Violation> {
        let Some(asked) = self.desk.calls.iter().find(|asked| asked.call == call) else {
            return Err(broke(
                Rule::Tools,
                format!("call {call} was sent, and it is not open"),
            ));
        };
        let effect = self.spec(asked.tool).map(|spec| spec.effect);
        let cut = self
            .desk
            .batches
            .iter()
            .any(|open| open.batch == asked.batch && open.cut);
        ensure(!cut, Rule::Liveness, || {
            format!(
                "call {call} was sent for response {}, which had been cut off",
                asked.by
            )
        })?;
        ensure(self.lifecycle == Lifecycle::Running, Rule::Closing, || {
            format!(
                "call {call} was sent while the session is {:?}",
                self.lifecycle
            )
        })?;
        let Some(cleared) = asked.cleared else {
            return Err(broke(
                Rule::Tools,
                format!("call {call} was sent without having been cleared"),
            ));
        };
        let stored = effect == Some(Effect::Pure) || cleared <= self.desk.durable;
        ensure(stored, Rule::Tools, || {
            format!(
                "call {call} may change something and was sent before fact #{cleared}, which \
                 cleared it, was stored; stored is up to #{}",
                self.desk.durable
            )
        })?;
        ensure(!asked.sent, Rule::Tools, || {
            format!("call {call} was sent twice")
        })?;
        // A call from an earlier life that was cleared then may have been
        // sent then.
        let again = asked.inherited;
        ensure(
            !again || effect != Some(Effect::SideEffecting),
            Rule::Tools,
            || format!("call {call} must happen at most once, and was sent again after a restart"),
        )?;
        let expected = if asked.dispatched {
            asked.attempt.saturating_add(1)
        } else {
            asked.attempt
        };
        ensure(attempt == expected, Rule::Tools, || {
            format!("call {call} was sent as attempt {attempt}; it is attempt {expected}")
        })?;
        let batch = asked.batch;
        if let Some(asked) = self.desk.calls.iter_mut().find(|asked| asked.call == call) {
            asked.sent = true;
            asked.dispatched = true;
            asked.attempt = attempt;
        }
        if let Some(open) = self
            .desk
            .batches
            .iter_mut()
            .find(|open| open.batch == batch)
        {
            open.ran = true;
        }
        self.desk.sending.push((call, attempt));
        Ok(())
    }

    pub(super) fn call_ended(
        &mut self,
        event: &Event,
        call: ToolCallId,
        outcome: ToolOutcome,
        text: &str,
    ) -> Result<(), Violation> {
        self.context += 1;
        let Some(index) = self.desk.calls.iter().position(|asked| asked.call == call) else {
            return Err(broke(
                Rule::Tools,
                format!("call {call} ended as {outcome:?}, and it is not open: a call ends once"),
            ));
        };
        let asked = self.desk.calls.remove(index);
        let spec = self.spec(asked.tool);
        let harmless = spec.is_none_or(|spec| spec.effect == Effect::Pure);
        let external = spec.is_some_and(|spec| spec.executor == Executor::External);
        let cut = self.is_cut(asked.batch, asked.by);
        let was_sent = asked.dispatched;
        let may_have_run = was_sent || (asked.inherited && asked.cleared.is_some());
        let worker = self
            .responses
            .iter()
            .position(|spoken| spoken.works_on == Some(call));
        let stopping = self.lifecycle >= Lifecycle::Closing;

        let holds = match outcome {
            ToolOutcome::Completed if external => {
                was_sent
                    && matches!(event, Event::Tool(of, ToolEvent::Completed { result })
                        if *of == call && result.as_str() == text)
            }
            ToolOutcome::Completed => {
                // What the agent working on it wrote, and nothing else.
                let written = worker.and_then(|index| self.responses.get(index));
                was_sent
                    && !text.is_empty()
                    && written.is_some_and(|spoken| {
                        spoken.script.trim() == text
                            && matches!(event, Event::Op(tag, _) if *tag == spoken.tag)
                    })
            }
            ToolOutcome::Failed => {
                let said = matches!(event, Event::Tool(of, ToolEvent::Failed { message, .. })
                    if *of == call && message.as_str() == text);
                // A call that may have changed something has failed only
                // if its tool says so.
                !external || !may_have_run || harmless || (said && was_sent)
            }
            // Only a call that was never sent is withdrawn.
            ToolOutcome::Cancelled if !may_have_run => cut || self.lifecycle != Lifecycle::Running,
            ToolOutcome::Cancelled => {
                let undone = asked.stopped
                    && matches!(event, Event::Tool(of, ToolEvent::RolledBack) if *of == call);
                undone || (harmless && stopping)
            }
            ToolOutcome::InDoubt => {
                may_have_run && !harmless && (asked.stopped || stopping || self.desk.recovering)
            }
        };
        ensure(holds, Rule::Tools, || {
            format!(
                "call {call} ended as {outcome:?} with {text:?} on {event:?}; it was sent: \
                 {was_sent}, told to stop: {}, its response was cut off: {cut}, the session is \
                 {:?}",
                asked.stopped, self.lifecycle
            )
        })?;

        let batch = self
            .desk
            .batches
            .iter_mut()
            .find(|open| open.batch == asked.batch);
        let Some(batch) = batch.filter(|open| open.pending > 0) else {
            return Err(broke(
                Rule::Tools,
                format!(
                    "call {call} ended, and its batch {} has no call left to end",
                    asked.batch
                ),
            ));
        };
        batch.pending -= 1;
        batch.ran |= outcome != ToolOutcome::Cancelled;
        if let Some(index) = worker {
            // The agent that worked on it has nothing left to work on, and
            // its language model is not left writing for nobody.
            let spoken = self.responses.remove(index);
            let told = self.desk.stopping.contains(&spoken.tag);
            ensure(spoken.done || told, Rule::Tools, || {
                format!(
                    "call {call} ended, and the language model of {:?}, which worked on it, was \
                     left writing",
                    spoken.tag
                )
            })?;
            if told {
                self.desk.retired.push(spoken.tag);
            }
        }
        self.desk.ended.push(asked);
        Ok(())
    }

    /// Whether `tag` worked on a call that ended in this step, and this is
    /// the one time its language model is told to stop.
    pub(super) fn retires(&mut self, tag: OpTag) -> bool {
        let index = self.desk.retired.iter().position(|&of| of == tag);
        index.map(|index| self.desk.retired.remove(index)).is_some()
    }

    pub(super) fn batch_resolved(&mut self, batch: BatchId) -> Result<(), Violation> {
        let Some(index) = self
            .desk
            .batches
            .iter()
            .position(|open| open.batch == batch)
        else {
            return Err(broke(
                Rule::Tools,
                format!("batch {batch} was resolved, and it is not open"),
            ));
        };
        let open = self.desk.batches.remove(index);
        let ending = self
            .desk
            .ending
            .iter()
            .any(|&(generation, _)| generation == open.by);
        let writing = !open.over
            && !ending
            && self.responses.iter().any(|spoken| {
                spoken.tag.generation == open.by
                    && !spoken.done
                    && !self.desk.stopping.contains(&spoken.tag)
            });
        ensure(open.pending == 0 && !writing, Rule::Tools, || {
            format!(
                "batch {batch} was resolved with {} calls running, and its response still \
                 writing: {writing}",
                open.pending
            )
        })?;
        if open.ran {
            // Its results are owed a response.
            self.owed = self.context;
            self.desk.cause = Some(LlmCause::Continuation { batch });
        }
        Ok(())
    }

    /// A command to a tool.
    pub(super) fn tool_command(
        &mut self,
        call: ToolCallId,
        command: ToolCmd,
        text: &str,
    ) -> Result<(), Violation> {
        let desk = &mut self.desk;
        let known = desk
            .calls
            .iter_mut()
            .chain(desk.ended.iter_mut())
            .find(|asked| asked.call == call);
        let Some(asked) = known else {
            return Err(broke(
                Rule::Tools,
                format!("{command:?} was sent for call {call}, which is not open"),
            ));
        };
        let external = self
            .config
            .tool(asked.tool)
            .is_some_and(|spec| spec.executor == Executor::External);
        match command {
            ToolCmd::Dispatch { tool, attempt, .. } => {
                let recorded = desk
                    .sending
                    .iter()
                    .position(|&sent| sent == (call, attempt));
                let same = asked.tool == tool && asked.args == text && external;
                ensure(recorded.is_some() && same, Rule::Tools, || {
                    format!(
                        "call {call} was sent to tool {tool} with {text:?} as attempt {attempt}; \
                         it is a call to tool {} with {:?}, and a record of sending it was \
                         emitted: {}",
                        asked.tool,
                        asked.args,
                        recorded.is_some()
                    )
                })?;
                if let Some(index) = recorded {
                    desk.sending.remove(index);
                }
            }
            ToolCmd::Cancel => {
                let may = asked.dispatched && external && !asked.stopped;
                asked.stopped = true;
                ensure(may, Rule::Tools, || {
                    format!("call {call} was told to stop, which makes no sense now")
                })?;
            }
        }
        Ok(())
    }

    pub(super) fn storing_asked(&mut self, seq: u64) -> Result<(), Violation> {
        let wanted = self.desk.awaiting.iter().position(|&record| record == seq);
        let Some(index) = wanted else {
            return Err(broke(
                Rule::Tools,
                format!("a report was asked for when fact #{seq} is stored, which clears no call"),
            ));
        };
        self.desk.awaiting.remove(index);
        Ok(())
    }

    /// A language model was started. Returns the call it works on, if it
    /// does not hold the floor.
    pub(super) fn started(
        &mut self,
        tag: OpTag,
        agent: AgentId,
        cause: LlmCause,
    ) -> Result<Option<ToolCallId>, Violation> {
        let LlmCause::Delegated { call } = cause else {
            let owed = self.desk.cause == Some(cause);
            let on_floor = agent == self.desk.front && self.desk.handoff.is_none();
            ensure(owed && on_floor, Rule::Liveness, || {
                format!(
                    "{tag:?} was started by agent {agent} for {cause:?}; what is owed is for \
                     {:?}, agent {} has the floor, and it is going to {:?}",
                    self.desk.cause, self.desk.front, self.desk.handoff
                )
            })?;
            if let LlmCause::Continuation { batch } = cause {
                let first = !self.desk.continued.contains(&batch);
                ensure(first, Rule::Continuation, || {
                    format!("a second continuation was started for batch {batch}")
                })?;
                self.desk.continued.push(batch);
            }
            return Ok(None);
        };
        let recorded = self.desk.sending.iter().position(|&(sent, _)| sent == call);
        let worker = self
            .desk
            .calls
            .iter()
            .chain(&self.desk.ended)
            .find(|asked| asked.call == call)
            .and_then(|asked| self.config.tool(asked.tool))
            .is_some_and(|spec| spec.executor == Executor::Agent(agent));
        ensure(recorded.is_some() && worker, Rule::Tools, || {
            format!(
                "agent {agent} was started on call {call} with {tag:?}; a record of sending the \
                 call was emitted: {}, and the call is the agent's to work on: {worker}",
                recorded.is_some()
            )
        })?;
        if let Some(index) = recorded {
            self.desk.sending.remove(index);
        }
        Ok(Some(call))
    }

    pub(super) fn handoff_asked(&mut self, event: &Event, to: AgentId) -> Result<(), Violation> {
        let asked = matches!(event, Event::Control(ControlInput::Handoff { to: wanted, .. })
            if *wanted == to);
        let may = self.lifecycle == Lifecycle::Running
            && self.desk.handoff.is_none()
            && to != self.desk.front
            && self.config.agent(to).is_some();
        ensure(asked && may, Rule::Stage, || {
            format!(
                "the floor was to go to agent {to} on {event:?}; agent {} has it, it is going to \
                 {:?}, and the session is {:?}",
                self.desk.front, self.desk.handoff, self.lifecycle
            )
        })?;
        self.desk.handoff = Some(to);
        Ok(())
    }

    pub(super) fn agent_activated(
        &mut self,
        event: &Event,
        agent: AgentId,
    ) -> Result<(), Violation> {
        let ready =
            matches!(event, Event::Control(ControlInput::StageReady)) || self.desk.recovering;
        let holds = match self.desk.handoff {
            // It became ready, or the agent that had the floor keeps it.
            Some(to) => (agent == to && ready) || agent == self.desk.front,
            None => false,
        };
        ensure(holds, Rule::Stage, || {
            format!(
                "agent {agent} was given the floor on {event:?}; agent {} has it, and it was \
                 going to {:?}",
                self.desk.front, self.desk.handoff
            )
        })?;
        self.desk.front = agent;
        self.desk.handoff = None;
        Ok(())
    }

    pub(super) fn noted(
        &mut self,
        event: &Event,
        kind: NoticeKind,
        text: &str,
    ) -> Result<(), Violation> {
        self.context += 1;
        match kind {
            NoticeKind::Recovered => {
                let first = self
                    .desk
                    .recovery
                    .as_mut()
                    .is_some_and(|recovery| !core::mem::replace(&mut recovery.noted, true));
                ensure(self.desk.recovering && first, Rule::Restore, || {
                    "a note that the session was restored was left when it was not, or twice"
                        .to_owned()
                })
            }
            NoticeKind::Handoff => {
                let given = match event {
                    Event::Control(ControlInput::Handoff { context, .. }) => match context {
                        HandoffContext::Full => None,
                        HandoffContext::Summary(summary) => Some(summary.as_str()),
                        HandoffContext::None => Some(""),
                    },
                    _ => None,
                };
                ensure(given == Some(text), Rule::Stage, || {
                    format!(
                        "a note for the agent taking over says {text:?}; the event is {event:?}"
                    )
                })
            }
        }
    }

    /// The speech of a response this life of the session never saw.
    pub(super) fn lost_speech(
        &mut self,
        generation: GenId,
        text: &str,
        truncated: bool,
        answers: bool,
    ) -> Result<(), Violation> {
        let speech = self
            .desk
            .recovery
            .as_mut()
            .and_then(|recovery| recovery.speech.take());
        let holds = self.desk.recovering
            && truncated
            && !answers
            && speech
                .as_ref()
                .is_some_and(|(of, heard)| *of == generation && heard.trim_end() == text);
        ensure(holds, Rule::SpokenText, || {
            format!(
                "response {generation} entered the conversation as {text:?}, and it is not under \
                 way; the speech lost with the session's earlier life is {speech:?}"
            )
        })
    }

    /// The first step of a session that was rebuilt begins.
    pub(super) fn recovery_began(&mut self, first: Option<bool>) -> Result<(), Violation> {
        self.desk.recovering = self.desk.recovery.is_some();
        ensure(
            !self.desk.recovering || first == Some(true),
            Rule::Restore,
            || "a session that was rebuilt did not begin by advancing the epoch".to_owned(),
        )
    }

    /// Compares the account of the tools with the kernel's own state after
    /// a step, and forgets what was known of the step only.
    pub(super) fn desk_settled(&mut self, view: View<'_>) -> Result<(), Violation> {
        let desk = &self.desk;
        ensure(desk.sending.is_empty(), Rule::Tools, || {
            format!(
                "the calls {:?} were recorded as sent, and nothing was sent",
                desk.sending
            )
        })?;
        ensure(desk.awaiting.is_empty(), Rule::Tools, || {
            format!(
                "calls that may change something were cleared by the facts {:?}, and no report \
                 was asked for when those are stored",
                desk.awaiting
            )
        })?;
        for open in &desk.batches {
            let unsent = desk
                .calls
                .iter()
                .find(|asked| asked.batch == open.batch && !asked.dispatched);
            ensure(!open.cut || unsent.is_none(), Rule::Tools, || {
                format!(
                    "response {} was cut off, and its call {:?} is still waiting to be sent",
                    open.by,
                    unsent.map(|asked| asked.call)
                )
            })?;
        }

        let state = |asked: &Asked| match (asked.cleared, asked.dispatched) {
            (None, _) => CallState::Proposed,
            (Some(_), false) => CallState::Authorized,
            (Some(_), true) => {
                let harmless = self
                    .config
                    .tool(asked.tool)
                    .is_none_or(|spec| spec.effect == Effect::Pure);
                if asked.stopped && !harmless {
                    CallState::Cancelling
                } else {
                    CallState::Dispatched
                }
            }
        };
        let seen = desk
            .calls
            .iter()
            .map(|asked| (asked.call, asked.batch, state(asked), asked.attempt))
            .collect::<Vec<_>>();
        let open = view
            .calls()
            .map(|open| (open.call, open.batch, open.state, open.attempt))
            .collect::<Vec<_>>();
        ensure(open == seen, Rule::Tools, || {
            format!("the kernel has the calls {open:?} open; its emissions say {seen:?}")
        })?;
        let seen = desk
            .batches
            .iter()
            .map(|open| (open.batch, open.by, open.pending, open.ran))
            .collect::<Vec<_>>();
        let open = view
            .batches()
            .map(|open| (open.batch, open.by, open.pending, open.ran))
            .collect::<Vec<_>>();
        ensure(open == seen, Rule::Tools, || {
            format!("the kernel has the batches {open:?} open; its facts say {seen:?}")
        })?;
        let floor = (view.front(), view.handoff());
        ensure(floor == (desk.front, desk.handoff), Rule::Stage, || {
            format!(
                "the kernel has agent {} on the floor and {:?} to take it; its facts say {} and \
                 {:?}",
                floor.0, floor.1, desk.front, desk.handoff
            )
        })?;
        ensure(
            view.owed().is_none_or(|cause| Some(cause) == desk.cause),
            Rule::Progress,
            || {
                format!(
                    "the kernel owes a response for {:?}; its facts say {:?}",
                    view.owed(),
                    desk.cause
                )
            },
        )?;
        ensure(desk.retired.is_empty(), Rule::Tools, || {
            format!(
                "the language models of {:?} were left writing for calls that ended",
                desk.retired
            )
        })?;
        self.recovery_settled()?;
        let desk = &mut self.desk;
        desk.ending.clear();
        desk.ended.clear();
        desk.stopping.clear();
        Ok(())
    }

    /// What the first step of a session that was rebuilt has to have
    /// done.
    fn recovery_settled(&mut self) -> Result<(), Violation> {
        let desk = &mut self.desk;
        if !core::mem::take(&mut desk.recovering) {
            return Ok(());
        }
        let Some(recovery) = desk.recovery.take() else {
            return Ok(());
        };
        ensure(recovery.speech.is_none(), Rule::SpokenText, || {
            format!(
                "{:?} was being heard when the session stopped, and was not recorded",
                recovery.speech
            )
        })?;
        ensure(recovery.lost == recovery.noted, Rule::Restore, || {
            format!(
                "work was lost with the session's earlier life: {}; a note of it was left: {}",
                recovery.lost, recovery.noted
            )
        })
    }

    /// What must hold of the tools once nothing more is going to happen.
    pub(super) fn desk_at_rest(&self) -> Result<(), Violation> {
        let desk = &self.desk;
        ensure(desk.calls.is_empty(), Rule::Progress, || {
            let calls = desk
                .calls
                .iter()
                .map(|asked| asked.call.get())
                .collect::<Vec<_>>();
            format!("the calls {calls:?} never ended")
        })?;
        ensure(desk.batches.is_empty(), Rule::Progress, || {
            let batches = desk
                .batches
                .iter()
                .map(|open| open.batch.get())
                .collect::<Vec<_>>();
            format!("the batches {batches:?} were never resolved")
        })?;
        ensure(desk.handoff.is_none(), Rule::Progress, || {
            format!("the floor never went to agent {:?}, nor back", desk.handoff)
        })
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use waveport_domain::command::{Command, LlmCause, LlmCmd, ToolCmd};
    use waveport_domain::config::{AgentSpec, Effect, Executor, Gate, KernelConfig, ToolSpec};
    use waveport_domain::event::{
        ControlInput, Event, Finish, HandoffContext, OpEvent, TimerId, ToolEvent, VadEdge,
    };
    use waveport_domain::fact::{EpochCause, Fact, Stamped, ToolOutcome};
    use waveport_domain::ids::{
        AgentId, BatchId, Epoch, GenId, OpTag, ParticipantId, Prob, Seq, ToolCallId, ToolId, TurnId,
    };
    use waveport_domain::kernel::Kernel;
    use waveport_domain::out::Out;
    use waveport_domain::policy::DefaultPolicies;
    use waveport_domain::time::{MediaSpan, MediaTime, MonoTime};

    use crate::invariants::{Journal, Monitor, Rule, Violation};

    const ANA: ParticipantId = ParticipantId(1);
    /// Changes nothing.
    const LOOKUP: ToolId = ToolId(0);
    /// Must happen at most once.
    const REFUND: ToolId = ToolId(1);
    /// Must happen at most once, and only after the agent has spoken.
    const HANG_UP: ToolId = ToolId(2);
    /// Answered by [`RESEARCHER`].
    const RESEARCH: ToolId = ToolId(3);
    const RESEARCHER: AgentId = AgentId(1);
    const BILLING: AgentId = AgentId(2);
    const REPLY: OpTag = OpTag {
        generation: GenId(1),
        epoch: Epoch(0),
    };
    const CALL: ToolCallId = ToolCallId(1);
    /// An event that makes the kernel do nothing.
    const NOTHING: Event = Event::Timer(TimerId::StageAck);

    fn desk() -> KernelConfig {
        let tools = vec![
            ToolSpec::new(Effect::Pure),
            ToolSpec::new(Effect::SideEffecting),
            ToolSpec {
                gate: Gate::AfterOwnUtterance,
                ..ToolSpec::new(Effect::SideEffecting)
            },
            ToolSpec {
                executor: Executor::Agent(RESEARCHER),
                timeout: Duration::from_secs(30),
                ..ToolSpec::new(Effect::Pure)
            },
        ];
        let all = vec![LOOKUP, REFUND, HANG_UP, RESEARCH];
        KernelConfig {
            tools,
            agents: vec![
                AgentSpec { tools: all.clone() },
                AgentSpec::default(),
                AgentSpec { tools: all },
            ],
            ..KernelConfig::default()
        }
    }

    /// A kernel with tools, a monitor watching it, and its journal.
    struct Watched {
        kernel: Kernel,
        monitor: Monitor,
        journal: Journal,
        out: Out,
        now_ms: u64,
    }

    impl Watched {
        /// Ana has asked something, and the agent's answer is being
        /// written.
        fn new() -> Self {
            let kernel = Kernel::new(desk(), DefaultPolicies::default()).unwrap();
            let mut watched = Self {
                monitor: Monitor::new(kernel.view()),
                journal: Journal::new(kernel.snapshot()),
                kernel,
                out: Out::new(),
                now_ms: 0,
            };
            let at = MediaTime::from_millis;
            let edge = |edge, millis| Event::Speech {
                who: ANA,
                edge,
                at: at(millis),
            };
            let asks = [
                (0, Event::Control(ControlInput::Join { who: ANA })),
                (0, edge(VadEdge::Start, 0)),
                (900, edge(VadEdge::End, 900)),
                (
                    960,
                    Event::Eou {
                        turn: TurnId(1),
                        p: Prob::ONE,
                    },
                ),
                (
                    1_100,
                    Event::Transcript {
                        who: ANA,
                        span: MediaSpan::new(at(0), at(900)),
                        text: "can you help me".into(),
                        is_final: true,
                    },
                ),
            ];
            for (millis, event) in asks {
                watched.now_ms = millis;
                watched.step(event).unwrap();
            }
            watched
        }

        fn step(&mut self, event: Event) -> Result<(), Violation> {
            self.tampered(event, |_| {})
        }

        /// Steps the kernel, journals what it emitted, and shows the
        /// monitor what `tamper` makes of that.
        #[expect(
            clippy::needless_pass_by_value,
            reason = "each call builds its event in place"
        )]
        fn tampered(
            &mut self,
            event: Event,
            tamper: impl FnOnce(&mut Out),
        ) -> Result<(), Violation> {
            let now = MonoTime::from_millis(self.now_ms);
            self.kernel.step(now, &event, &mut self.out);
            self.journal.record(&self.out);
            tamper(&mut self.out);
            self.monitor
                .observe(now, &event, &self.out, self.kernel.view())
        }

        /// The model writing the answer asks for a call to `tool`.
        fn proposes(tool: ToolId) -> Event {
            let args = "{}".into();
            Event::Op(REPLY, OpEvent::ToolProposed { tool, args })
        }

        /// The model writing the answer finishes, having said nothing.
        fn done() -> Event {
            let finish = Finish::ToolCalls;
            Event::Op(REPLY, OpEvent::LlmDone { finish })
        }

        fn returns(result: &str) -> Event {
            let result = result.into();
            Event::Tool(CALL, ToolEvent::Completed { result })
        }

        /// The process dies and the session is rebuilt from its journal.
        fn restart(&mut self) {
            let (snapshot, tail, text) = self.journal.latest().unwrap();
            self.kernel =
                Kernel::restore(desk(), DefaultPolicies::default(), snapshot, tail, text).unwrap();
            self.monitor = Monitor::restored(self.kernel.view(), self.journal.speech());
        }
    }

    /// The place of the fact after the last one in `out`.
    fn next(out: &Out) -> Seq {
        out.facts.last().map_or(Seq(1), |last| last.seq.next())
    }

    fn broken(checked: Result<(), Violation>, rule: Rule, saying: &str) {
        let violation = checked.unwrap_err();
        assert_eq!(violation.rule, rule, "{violation}");
        assert!(violation.what.contains(saying), "{violation}");
    }

    #[test]
    fn an_honest_call_is_accepted_to_its_continuation() {
        let mut watched = Watched::new();
        assert_eq!(watched.step(Watched::proposes(LOOKUP)), Ok(()));
        assert_eq!(watched.step(Watched::proposes(RESEARCH)), Ok(()));
        assert_eq!(watched.step(Watched::done()), Ok(()));
        assert_eq!(watched.step(Watched::returns("A-1041")), Ok(()));
        // A second result is dropped, and the monitor has nothing to say.
        assert_eq!(watched.step(Watched::returns("A-1041")), Ok(()));
        let worker = OpTag {
            generation: GenId(2),
            epoch: Epoch(0),
        };
        let text = "It left on Monday.".into();
        assert_eq!(
            watched.step(Event::Op(worker, OpEvent::LlmDelta { text })),
            Ok(())
        );
        let finish = Finish::Stop;
        assert_eq!(
            watched.step(Event::Op(worker, OpEvent::LlmDone { finish })),
            Ok(())
        );
        let continued = watched.out.cmds.iter().any(|command| {
            let batch = BatchId(1);
            matches!(command, Command::Llm(_, LlmCmd::Start { cause, .. })
                if *cause == LlmCause::Continuation { batch })
        });
        assert!(continued, "{:?}", watched.out.cmds);
        assert_eq!(watched.journal.check(&watched.kernel), Ok(()));
    }

    #[test]
    fn a_call_recorded_as_sent_and_never_sent_is_noticed() {
        let mut watched = Watched::new();
        let checked = watched.tampered(Watched::proposes(LOOKUP), |out| {
            out.cmds
                .retain(|command| !matches!(command, Command::Tool(..)));
        });
        broken(checked, Rule::Tools, "nothing was sent");
    }

    #[test]
    fn a_call_sent_before_its_record_is_stored_is_noticed() {
        let mut watched = Watched::new();
        let checked = watched.tampered(Watched::proposes(REFUND), |out| {
            let attempt = 0;
            let fact = Fact::ToolDispatched {
                call: CALL,
                attempt,
            };
            let seq = next(out);
            out.facts.push(Stamped { seq, fact });
            let args = out.text.push("{}");
            let tool = REFUND;
            let send = ToolCmd::Dispatch {
                tool,
                args,
                attempt,
            };
            out.cmds.push(Command::Tool(CALL, send));
        });
        broken(checked, Rule::Tools, "was sent before fact #");

        // Once the journal says so, it is sent.
        let mut watched = Watched::new();
        assert_eq!(watched.step(Watched::proposes(REFUND)), Ok(()));
        let cleared = watched.journal.last();
        assert_eq!(watched.step(Event::Durable(cleared)), Ok(()));
        assert_eq!(watched.kernel.view().calls().count(), 1);
        assert!(
            watched
                .out
                .cmds
                .iter()
                .any(|command| matches!(command, Command::Tool(..)))
        );
    }

    #[test]
    fn a_record_whose_storing_is_not_waited_for_is_noticed() {
        let mut watched = Watched::new();
        let checked = watched.tampered(Watched::proposes(REFUND), |out| {
            out.cmds
                .retain(|command| !matches!(command, Command::AwaitDurable(_)));
        });
        broken(checked, Rule::Tools, "no report was asked for");
    }

    #[test]
    fn a_gate_that_is_skipped_is_noticed() {
        let mut watched = Watched::new();
        let checked = watched.tampered(Watched::proposes(HANG_UP), |out| {
            let fact = Fact::ToolAuthorized { call: CALL };
            let seq = next(out);
            out.facts.push(Stamped { seq, fact });
        });
        broken(checked, Rule::Tools, "its gate is open: false");
    }

    #[test]
    fn a_call_that_ends_twice_is_noticed() {
        let mut watched = Watched::new();
        watched.step(Watched::proposes(LOOKUP)).unwrap();
        let checked = watched.tampered(Watched::returns("A-1041"), |out| {
            let again = out.facts.first().copied().unwrap();
            let seq = next(out);
            out.facts.push(Stamped { seq, ..again });
        });
        broken(checked, Rule::Tools, "a call ends once");
    }

    #[test]
    fn a_result_its_tool_did_not_give_is_noticed() {
        let mut watched = Watched::new();
        watched.step(Watched::proposes(LOOKUP)).unwrap();
        let checked = watched.tampered(Watched::returns("A-1041"), |out| {
            let other = out.text.push("A-1042");
            for stamped in &mut out.facts {
                if let Fact::ToolFinished { result, .. } = &mut stamped.fact {
                    *result = other;
                }
            }
        });
        broken(checked, Rule::Tools, "ended as Completed with \"A-1042\"");
    }

    #[test]
    fn a_call_withdrawn_after_it_was_sent_is_noticed() {
        let mut watched = Watched::new();
        watched.step(Watched::proposes(LOOKUP)).unwrap();
        let checked = watched.tampered(Watched::returns("A-1041"), |out| {
            for stamped in &mut out.facts {
                if let Fact::ToolFinished { outcome, .. } = &mut stamped.fact {
                    *outcome = ToolOutcome::Cancelled;
                }
            }
        });
        broken(checked, Rule::Tools, "ended as Cancelled");
    }

    #[test]
    fn a_call_cleared_for_a_response_that_was_cut_off_is_noticed() {
        let mut watched = Watched::new();
        watched.step(Watched::proposes(HANG_UP)).unwrap();
        let interrupt = Event::Control(ControlInput::Interrupt);
        // The kernel withdraws the call, then advances the epoch. Here the
        // call is cleared instead, once the response is gone.
        let checked = watched.tampered(interrupt, |out| {
            let first = out.facts.first().map_or(1, |first| first.seq.get());
            let cleared = Fact::ToolAuthorized { call: CALL };
            let mut facts = out.facts.iter().map(|stamped| match stamped.fact {
                Fact::ToolFinished { .. } => cleared,
                other => other,
            });
            let withdrawn = facts.next().unwrap();
            let epoch = facts.next().unwrap();
            let rest = facts.collect::<Vec<_>>();
            assert!(matches!(epoch, Fact::EpochAdvanced { .. }), "{epoch:?}");
            let reordered = [epoch, withdrawn].into_iter().chain(rest);
            out.facts = (first..)
                .zip(reordered)
                .map(|(number, fact)| Stamped {
                    seq: Seq(number),
                    fact,
                })
                .collect();
        });
        broken(checked, Rule::Tools, "its response was cut off: true");
    }

    #[test]
    fn a_second_continuation_is_noticed() {
        let mut watched = Watched::new();
        watched.step(Watched::proposes(LOOKUP)).unwrap();
        watched.step(Watched::done()).unwrap();
        watched.step(Watched::returns("A-1041")).unwrap();
        let started = watched.out.cmds.iter().find_map(|command| match *command {
            Command::Llm(tag, start @ LlmCmd::Start { .. }) => Some((tag, start)),
            _ => None,
        });
        let (tag, start) = started.unwrap();
        let checked = watched.tampered(NOTHING, |out| {
            let generation = GenId(tag.generation.get() + 1);
            let other = OpTag { generation, ..tag };
            out.cmds.push(Command::Llm(other, start));
        });
        broken(checked, Rule::Continuation, "a second continuation");
    }

    #[test]
    fn an_agent_left_working_on_a_call_that_ended_is_noticed() {
        let mut watched = Watched::new();
        watched.step(Watched::proposes(RESEARCH)).unwrap();
        watched.now_ms += 30_000;
        let late = Event::Timer(TimerId::ToolTimeout(CALL));
        let checked = watched.tampered(late, |out| {
            out.cmds
                .retain(|command| !matches!(command, Command::Llm(_, LlmCmd::Cancel)));
        });
        broken(checked, Rule::Tools, "was left writing");
    }

    #[test]
    fn the_floor_given_to_the_wrong_agent_is_noticed() {
        let mut watched = Watched::new();
        let to = BILLING;
        let context = HandoffContext::Full;
        let handoff = Event::Control(ControlInput::Handoff { to, context });
        assert_eq!(watched.step(handoff), Ok(()));
        let ready = Event::Control(ControlInput::StageReady);
        let checked = watched.tampered(ready, |out| {
            for stamped in &mut out.facts {
                if let Fact::AgentActivated { agent } = &mut stamped.fact {
                    *agent = RESEARCHER;
                }
            }
        });
        broken(checked, Rule::Stage, "agent 1 was given the floor");
    }

    #[test]
    fn a_session_rebuilt_from_its_journal_is_watched_from_its_first_step() {
        let attached = || Event::Control(ControlInput::Attached);
        let mut watched = Watched::new();
        watched.step(Watched::proposes(LOOKUP)).unwrap();
        watched.restart();
        assert_eq!(watched.step(attached()), Ok(()));
        assert_eq!(watched.journal.check(&watched.kernel), Ok(()));

        // A first step that does not say that the session was restarted.
        let mut watched = Watched::new();
        watched.step(Watched::proposes(LOOKUP)).unwrap();
        watched.restart();
        let checked = watched.tampered(attached(), |out| {
            for stamped in &mut out.facts {
                if let Fact::EpochAdvanced { cause, .. } = &mut stamped.fact {
                    *cause = EpochCause::Interrupted;
                }
            }
        });
        broken(
            checked,
            Rule::Restore,
            "did not begin by advancing the epoch",
        );

        // One that sends again a call that must happen at most once.
        let mut watched = Watched::new();
        watched.step(Watched::proposes(REFUND)).unwrap();
        let cleared = watched.journal.last();
        watched.step(Event::Durable(cleared)).unwrap();
        watched.restart();
        let checked = watched.tampered(attached(), |out| {
            let attempt = 1;
            for stamped in &mut out.facts {
                if matches!(stamped.fact, Fact::ToolFinished { .. }) {
                    stamped.fact = Fact::ToolDispatched {
                        call: CALL,
                        attempt,
                    };
                }
            }
            let args = out.text.push("{}");
            let tool = REFUND;
            let send = ToolCmd::Dispatch {
                tool,
                args,
                attempt,
            };
            out.cmds.push(Command::Tool(CALL, send));
        });
        broken(checked, Rule::Tools, "was sent again after a restart");
    }

    #[test]
    fn a_journal_that_does_not_add_up_to_the_session_is_noticed() {
        let mut watched = Watched::new();
        watched.step(Watched::proposes(LOOKUP)).unwrap();
        watched.journal.keep(watched.kernel.snapshot());
        watched.step(Watched::returns("A-1041")).unwrap();
        assert_eq!(watched.journal.check(&watched.kernel), Ok(()));

        // A crash takes the facts that were not stored, and the snapshots
        // that held them.
        let last = watched.journal.last();
        let kept = watched.kernel.snapshot().next_seq();
        watched.journal.keep(watched.kernel.snapshot());
        watched.journal.cut(Seq(last.get() - 1));
        assert_eq!(watched.journal.last(), Seq(last.get() - 1));
        let (snapshot, tail, _) = watched.journal.latest().unwrap();
        assert!(snapshot.next_seq() < kept);
        assert_eq!(tail.len(), 0);
        let checked = watched.journal.check(&watched.kernel);
        broken(checked, Rule::Restore, "add up to");
    }
}
