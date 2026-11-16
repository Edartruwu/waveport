//! Kernel facts as journal records, and back; and a kernel's snapshot as
//! the state a journal stores, and back.
//!
//! The kernel knows nothing of how a fact is stored, and the record format
//! in `waveport-protocol` knows nothing of the kernel. This module is where
//! the two meet. Every `match` here is exhaustive on purpose: a fact added
//! to the kernel does not compile until it has a record, and a record
//! added to the format does not compile until it has a fact. A snapshot is
//! taken apart and put together with every one of its values named, for
//! the same reason.

use core::fmt;
use std::borrow::Cow;

use bytes::Bytes;
use waveport_domain::command::LlmCause;
use waveport_domain::event::CloseReason;
use waveport_domain::fact::{
    Dismissal, EpochCause, Fact, Lifecycle, NoticeKind, Stamped, ToolOutcome,
};
use waveport_domain::ids::{
    AgentId, BatchId, ContextVersion, Epoch, GenId, ParticipantId, Seq, ToolCallId, ToolId, TurnId,
};
use waveport_domain::kernel::{
    BatchView, CallState, CallView, ItemView, Snapshot, SnapshotParts, SpeechHeard,
};
use waveport_domain::text::TextArena;
use waveport_domain::time::{MediaSpan, MediaTime};
use waveport_protocol::journal as wire;

/// Why a record could not be written or read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodecError {
    /// The bytes are not a record.
    Malformed(String),
    /// The record was written in a later version of the format than this
    /// build knows.
    Newer {
        /// The version the record says it was written in.
        version: u32,
    },
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "not a journal record: {why}"),
            Self::Newer { version } => write!(
                f,
                "the record is in version {version} of the format, and this build reads up to \
                 version {}",
                wire::VERSION
            ),
        }
    }
}

impl core::error::Error for CodecError {}

/// Writes a fact as a journal record. `text` is the arena the fact's text
/// refers to: the one of the step that emitted it.
///
/// # Errors
///
/// The record could not be written. That is a fault of this module and not
/// of the fact.
pub fn encode(stamped: &Stamped, text: &TextArena) -> Result<Bytes, CodecError> {
    let record = wire::Record {
        v: wire::VERSION,
        seq: stamped.seq.get(),
        fact: to_wire(&stamped.fact, text),
    };
    serde_json::to_vec(&record)
        .map(Bytes::from)
        .map_err(|error| CodecError::Malformed(error.to_string()))
}

/// Reads a journal record back. The fact's text is put into `text`, and
/// the fact refers to it there.
///
/// # Errors
///
/// The bytes are not a record, or the record is from a later version of
/// the format.
pub fn decode(record: &[u8], text: &mut TextArena) -> Result<Stamped, CodecError> {
    version_of(record)?;
    let record: wire::Record<'_> =
        serde_json::from_slice(record).map_err(|error| CodecError::Malformed(error.to_string()))?;
    Ok(Stamped {
        seq: Seq(record.seq),
        fact: from_wire(&record.fact, text)?,
    })
}

/// The version a record or a state says it was written in. It is looked
/// at first, and alone: what a later version wrote may hold what this
/// build cannot read at all.
fn version_of(written: &[u8]) -> Result<u32, CodecError> {
    #[derive(serde::Deserialize)]
    struct Versioned {
        v: u32,
    }
    let Versioned { v } = serde_json::from_slice(written)
        .map_err(|error| CodecError::Malformed(error.to_string()))?;
    if v > wire::VERSION {
        return Err(CodecError::Newer { version: v });
    }
    Ok(v)
}

/// Writes what a session's facts add up to as the state a journal stores
/// in their place. `heard_until` is where the last audio ends that a turn
/// among those facts was spoken in.
///
/// # Errors
///
/// The state could not be written. That is a fault of this module and not
/// of the snapshot.
pub fn encode_state(snapshot: &Snapshot, heard_until: MediaTime) -> Result<Bytes, CodecError> {
    let state = state_out(&snapshot.parts(), heard_until);
    serde_json::to_vec(&state)
        .map(Bytes::from)
        .map_err(|error| CodecError::Malformed(error.to_string()))
}

/// Reads a stored state back: the snapshot, and where the last audio ends
/// that a turn it includes was spoken in.
///
/// # Errors
///
/// The bytes are not a state, or the state is from a later version of the
/// format.
pub fn decode_state(state: &[u8]) -> Result<(Snapshot, MediaTime), CodecError> {
    version_of(state)?;
    let state: wire::State<'_> =
        serde_json::from_slice(state).map_err(|error| CodecError::Malformed(error.to_string()))?;
    let heard_until = MediaTime::from_ticks(state.heard_until);
    Ok((Snapshot::from_parts(&state_in(&state)), heard_until))
}

fn state_out<'a>(parts: &SnapshotParts<'a>, heard_until: MediaTime) -> wire::State<'a> {
    // Every value by its name, so that one added to a snapshot does not
    // compile until it is written.
    let SnapshotParts {
        epoch,
        lifecycle,
        closed_for,
        front,
        handoff,
        next_seq,
        turn_floor,
        gen_floor,
        next_call,
        next_batch,
        ref participants,
        ref calls,
        ref batches,
        ref items,
        owed,
        answered,
        speech,
    } = *parts;
    wire::State {
        v: wire::VERSION,
        upto: next_seq.get().saturating_sub(1),
        heard_until: heard_until.ticks(),
        epoch: epoch.get(),
        lifecycle: lifecycle_out(lifecycle),
        closed_for: closed_for.map(close_out),
        front: front.get(),
        handoff: handoff.map(AgentId::get),
        turn_floor: turn_floor.get(),
        gen_floor: gen_floor.get(),
        next_call: next_call.get(),
        next_batch: next_batch.get(),
        participants: participants.iter().map(|who| who.get()).collect(),
        calls: calls.iter().map(call_out).collect(),
        batches: batches.iter().map(batch_out).collect(),
        items: items.iter().map(item_out).collect(),
        owed: owed.map(|(version, cause)| wire::Owed {
            version: version.get(),
            cause: owed_out(cause),
        }),
        answered: answered.get(),
        speech: speech.map(|heard| wire::Speech {
            generation: heard.generation.get(),
            agent: heard.agent.get(),
            heard: Cow::Borrowed(heard.text),
        }),
    }
}

fn state_in<'a>(state: &'a wire::State<'_>) -> SnapshotParts<'a> {
    let wire::State {
        v: _,
        upto,
        heard_until: _,
        epoch,
        lifecycle,
        closed_for,
        front,
        handoff,
        turn_floor,
        gen_floor,
        next_call,
        next_batch,
        ref participants,
        ref calls,
        ref batches,
        ref items,
        owed,
        answered,
        ref speech,
    } = *state;
    SnapshotParts {
        epoch: Epoch(epoch),
        lifecycle: lifecycle_in(lifecycle),
        closed_for: closed_for.map(close_in),
        front: AgentId(front),
        handoff: handoff.map(AgentId),
        next_seq: Seq(upto.saturating_add(1)),
        turn_floor: TurnId(turn_floor),
        gen_floor: GenId(gen_floor),
        next_call: ToolCallId(next_call),
        next_batch: BatchId(next_batch),
        participants: participants.iter().copied().map(ParticipantId).collect(),
        calls: calls.iter().map(call_in).collect(),
        batches: batches.iter().map(batch_in).collect(),
        items: items.iter().map(item_in).collect(),
        owed: owed.map(|owed| (ContextVersion(owed.version), owed_in(owed.cause))),
        answered: ContextVersion(answered),
        speech: speech.as_ref().map(|speech| SpeechHeard {
            generation: GenId(speech.generation),
            agent: AgentId(speech.agent),
            text: &speech.heard,
        }),
    }
}

fn call_out(open: &CallView) -> wire::OpenCall {
    let CallView {
        call,
        tool,
        batch,
        by,
        state,
        attempt,
    } = *open;
    wire::OpenCall {
        call: call.get(),
        tool: tool.get(),
        batch: batch.get(),
        by: by.get(),
        state: match state {
            CallState::Proposed => wire::CallState::Proposed,
            CallState::Authorized => wire::CallState::Authorized,
            // A call that is being stopped was sent, and that is all a
            // journal knows of it.
            CallState::Dispatched | CallState::Cancelling => wire::CallState::Dispatched,
        },
        attempt,
    }
}

const fn call_in(open: &wire::OpenCall) -> CallView {
    let wire::OpenCall {
        call,
        tool,
        batch,
        by,
        state,
        attempt,
    } = *open;
    CallView {
        call: ToolCallId(call),
        tool: ToolId(tool),
        batch: BatchId(batch),
        by: GenId(by),
        state: match state {
            wire::CallState::Proposed => CallState::Proposed,
            wire::CallState::Authorized => CallState::Authorized,
            wire::CallState::Dispatched => CallState::Dispatched,
        },
        attempt,
    }
}

fn batch_out(open: &BatchView) -> wire::OpenBatch {
    let BatchView {
        batch,
        by,
        pending,
        ran,
    } = *open;
    wire::OpenBatch {
        batch: batch.get(),
        by: by.get(),
        pending: u32::try_from(pending).unwrap_or(u32::MAX),
        ran,
    }
}

fn batch_in(open: &wire::OpenBatch) -> BatchView {
    let wire::OpenBatch {
        batch,
        by,
        pending,
        ran,
    } = *open;
    BatchView {
        batch: BatchId(batch),
        by: GenId(by),
        pending: usize::try_from(pending).unwrap_or(usize::MAX),
        ran,
    }
}

fn item_out<'a>(item: &ItemView<'a>) -> wire::Item<'a> {
    match *item {
        ItemView::UserTurn {
            turn,
            who,
            text,
            addressed,
        } => wire::Item::UserTurn {
            turn: turn.get(),
            who: who.get(),
            text: Cow::Borrowed(text),
            addressed,
        },
        ItemView::Utterance {
            generation,
            agent,
            text,
            truncated,
        } => wire::Item::Utterance {
            generation: generation.get(),
            agent: agent.get(),
            text: Cow::Borrowed(text),
            truncated,
        },
        ItemView::ToolCall {
            call,
            tool,
            generation,
            args,
        } => wire::Item::ToolCall {
            call: call.get(),
            tool: tool.get(),
            generation: generation.get(),
            args: Cow::Borrowed(args),
        },
        ItemView::ToolResult {
            call,
            outcome,
            text,
        } => wire::Item::ToolResult {
            call: call.get(),
            outcome: outcome_out(outcome),
            text: Cow::Borrowed(text),
        },
        ItemView::Notice { kind, text } => wire::Item::Notice {
            about: notice_out(kind),
            text: Cow::Borrowed(text),
        },
    }
}

fn item_in<'a>(item: &'a wire::Item<'_>) -> ItemView<'a> {
    match *item {
        wire::Item::UserTurn {
            turn,
            who,
            ref text,
            addressed,
        } => ItemView::UserTurn {
            turn: TurnId(turn),
            who: ParticipantId(who),
            text,
            addressed,
        },
        wire::Item::Utterance {
            generation,
            agent,
            ref text,
            truncated,
        } => ItemView::Utterance {
            generation: GenId(generation),
            agent: AgentId(agent),
            text,
            truncated,
        },
        wire::Item::ToolCall {
            call,
            tool,
            generation,
            ref args,
        } => ItemView::ToolCall {
            call: ToolCallId(call),
            tool: ToolId(tool),
            generation: GenId(generation),
            args,
        },
        wire::Item::ToolResult {
            call,
            outcome,
            ref text,
        } => ItemView::ToolResult {
            call: ToolCallId(call),
            outcome: outcome_in(outcome),
            text,
        },
        wire::Item::Notice { about, ref text } => ItemView::Notice {
            kind: notice_in(about),
            text,
        },
    }
}

const fn owed_out(cause: LlmCause) -> wire::Cause {
    match cause {
        LlmCause::Reply { turn } => wire::Cause::Reply { turn: turn.get() },
        LlmCause::Continuation { batch } => wire::Cause::Continuation { batch: batch.get() },
        LlmCause::Delegated { call } => wire::Cause::Delegated { call: call.get() },
    }
}

const fn owed_in(cause: wire::Cause) -> LlmCause {
    match cause {
        wire::Cause::Reply { turn } => LlmCause::Reply { turn: TurnId(turn) },
        wire::Cause::Continuation { batch } => LlmCause::Continuation {
            batch: BatchId(batch),
        },
        wire::Cause::Delegated { call } => LlmCause::Delegated {
            call: ToolCallId(call),
        },
    }
}

fn to_wire<'a>(fact: &Fact, text: &'a TextArena) -> wire::Fact<'a> {
    let borrowed = |at| Cow::Borrowed(text.get(at));
    match *fact {
        Fact::ParticipantJoined { who } => wire::Fact::ParticipantJoined { who: who.get() },
        Fact::ParticipantLeft { who } => wire::Fact::ParticipantLeft { who: who.get() },
        Fact::LifecycleChanged { to, reason } => wire::Fact::LifecycleChanged {
            to: lifecycle_out(to),
            reason: reason.map(close_out),
        },
        Fact::EpochAdvanced { to, cause } => wire::Fact::EpochAdvanced {
            to: to.get(),
            cause: cause_out(cause),
        },
        Fact::TurnCommitted {
            turn,
            who,
            span,
            text: said,
            addressed,
        } => wire::Fact::TurnCommitted {
            turn: turn.get(),
            who: who.get(),
            span: [span.start.ticks(), span.end.ticks()],
            text: borrowed(said),
            addressed,
        },
        Fact::TurnDismissed { turn, who, reason } => wire::Fact::TurnDismissed {
            turn: turn.get(),
            who: who.get(),
            reason: dismissal_out(reason),
        },
        Fact::NoticeAppended { kind, text: note } => wire::Fact::NoticeAppended {
            about: notice_out(kind),
            text: borrowed(note),
        },
        Fact::UtteranceProgress {
            generation,
            agent,
            heard,
        } => wire::Fact::UtteranceProgress {
            generation: generation.get(),
            agent: agent.get(),
            heard: borrowed(heard),
        },
        Fact::UtteranceCommitted {
            generation,
            agent,
            answers,
            text: heard,
            truncated,
        } => wire::Fact::UtteranceCommitted {
            generation: generation.get(),
            agent: agent.get(),
            answers: answers.map(ContextVersion::get),
            text: borrowed(heard),
            truncated,
        },
        Fact::ToolProposed {
            call,
            batch,
            generation,
            tool,
            args,
        } => wire::Fact::ToolProposed {
            call: call.get(),
            batch: batch.get(),
            generation: generation.get(),
            tool: tool.get(),
            args: borrowed(args),
        },
        Fact::ToolAuthorized { call } => wire::Fact::ToolAuthorized { call: call.get() },
        Fact::ToolDispatched { call, attempt } => wire::Fact::ToolDispatched {
            call: call.get(),
            attempt,
        },
        Fact::ToolFinished {
            call,
            outcome,
            result,
        } => wire::Fact::ToolFinished {
            call: call.get(),
            outcome: outcome_out(outcome),
            result: borrowed(result),
        },
        Fact::BatchResolved { batch } => wire::Fact::BatchResolved { batch: batch.get() },
        Fact::HandoffRequested { to } => wire::Fact::HandoffRequested { to: to.get() },
        Fact::AgentActivated { agent } => wire::Fact::AgentActivated { agent: agent.get() },
    }
}

#[expect(clippy::too_many_lines, reason = "one arm for each kind of fact")]
fn from_wire(fact: &wire::Fact<'_>, text: &mut TextArena) -> Result<Fact, CodecError> {
    Ok(match *fact {
        wire::Fact::ParticipantJoined { who } => Fact::ParticipantJoined {
            who: ParticipantId(who),
        },
        wire::Fact::ParticipantLeft { who } => Fact::ParticipantLeft {
            who: ParticipantId(who),
        },
        wire::Fact::LifecycleChanged { to, reason } => Fact::LifecycleChanged {
            to: lifecycle_in(to),
            reason: reason.map(close_in),
        },
        wire::Fact::EpochAdvanced { to, cause } => Fact::EpochAdvanced {
            to: Epoch(to),
            cause: cause_in(cause),
        },
        wire::Fact::TurnCommitted {
            turn,
            who,
            span: [start, end],
            text: ref said,
            addressed,
        } => {
            if end < start {
                return Err(CodecError::Malformed(format!(
                    "a turn that ends at {end} before it begins at {start}"
                )));
            }
            Fact::TurnCommitted {
                turn: TurnId(turn),
                who: ParticipantId(who),
                span: MediaSpan::new(MediaTime::from_ticks(start), MediaTime::from_ticks(end)),
                text: text.push(said),
                addressed,
            }
        }
        wire::Fact::TurnDismissed { turn, who, reason } => Fact::TurnDismissed {
            turn: TurnId(turn),
            who: ParticipantId(who),
            reason: dismissal_in(reason),
        },
        wire::Fact::NoticeAppended {
            about,
            text: ref note,
        } => Fact::NoticeAppended {
            kind: notice_in(about),
            text: text.push(note),
        },
        wire::Fact::UtteranceProgress {
            generation,
            agent,
            ref heard,
        } => Fact::UtteranceProgress {
            generation: GenId(generation),
            agent: AgentId(agent),
            heard: text.push(heard),
        },
        wire::Fact::UtteranceCommitted {
            generation,
            agent,
            answers,
            text: ref heard,
            truncated,
        } => Fact::UtteranceCommitted {
            generation: GenId(generation),
            agent: AgentId(agent),
            answers: answers.map(ContextVersion),
            text: text.push(heard),
            truncated,
        },
        wire::Fact::ToolProposed {
            call,
            batch,
            generation,
            tool,
            ref args,
        } => Fact::ToolProposed {
            call: ToolCallId(call),
            batch: BatchId(batch),
            generation: GenId(generation),
            tool: ToolId(tool),
            args: text.push(args),
        },
        wire::Fact::ToolAuthorized { call } => Fact::ToolAuthorized {
            call: ToolCallId(call),
        },
        wire::Fact::ToolDispatched { call, attempt } => Fact::ToolDispatched {
            call: ToolCallId(call),
            attempt,
        },
        wire::Fact::ToolFinished {
            call,
            outcome,
            ref result,
        } => Fact::ToolFinished {
            call: ToolCallId(call),
            outcome: outcome_in(outcome),
            result: text.push(result),
        },
        wire::Fact::BatchResolved { batch } => Fact::BatchResolved {
            batch: BatchId(batch),
        },
        wire::Fact::HandoffRequested { to } => Fact::HandoffRequested { to: AgentId(to) },
        wire::Fact::AgentActivated { agent } => Fact::AgentActivated {
            agent: AgentId(agent),
        },
    })
}

const fn lifecycle_out(stage: Lifecycle) -> wire::Lifecycle {
    match stage {
        Lifecycle::Running => wire::Lifecycle::Running,
        Lifecycle::Draining => wire::Lifecycle::Draining,
        Lifecycle::Closing => wire::Lifecycle::Closing,
        Lifecycle::Closed => wire::Lifecycle::Closed,
    }
}

const fn lifecycle_in(stage: wire::Lifecycle) -> Lifecycle {
    match stage {
        wire::Lifecycle::Running => Lifecycle::Running,
        wire::Lifecycle::Draining => Lifecycle::Draining,
        wire::Lifecycle::Closing => Lifecycle::Closing,
        wire::Lifecycle::Closed => Lifecycle::Closed,
    }
}

const fn close_out(reason: CloseReason) -> wire::CloseReason {
    match reason {
        CloseReason::Requested => wire::CloseReason::Requested,
        CloseReason::Abandoned => wire::CloseReason::Abandoned,
        CloseReason::Failure => wire::CloseReason::Failure,
    }
}

const fn close_in(reason: wire::CloseReason) -> CloseReason {
    match reason {
        wire::CloseReason::Requested => CloseReason::Requested,
        wire::CloseReason::Abandoned => CloseReason::Abandoned,
        wire::CloseReason::Failure => CloseReason::Failure,
    }
}

const fn cause_out(cause: EpochCause) -> wire::EpochCause {
    match cause {
        EpochCause::BargeIn => wire::EpochCause::BargeIn,
        EpochCause::Interrupted => wire::EpochCause::Interrupted,
        EpochCause::Handoff => wire::EpochCause::Handoff,
        EpochCause::Superseded => wire::EpochCause::Superseded,
        EpochCause::Failure => wire::EpochCause::Failure,
        EpochCause::Recovery => wire::EpochCause::Recovery,
        EpochCause::Closing => wire::EpochCause::Closing,
    }
}

const fn cause_in(cause: wire::EpochCause) -> EpochCause {
    match cause {
        wire::EpochCause::BargeIn => EpochCause::BargeIn,
        wire::EpochCause::Interrupted => EpochCause::Interrupted,
        wire::EpochCause::Handoff => EpochCause::Handoff,
        wire::EpochCause::Superseded => EpochCause::Superseded,
        wire::EpochCause::Failure => EpochCause::Failure,
        wire::EpochCause::Recovery => EpochCause::Recovery,
        wire::EpochCause::Closing => EpochCause::Closing,
    }
}

const fn dismissal_out(reason: Dismissal) -> wire::Dismissal {
    match reason {
        Dismissal::Backchannel => wire::Dismissal::Backchannel,
        Dismissal::Noise => wire::Dismissal::Noise,
        Dismissal::Empty => wire::Dismissal::Empty,
        Dismissal::Closed => wire::Dismissal::Closed,
    }
}

const fn dismissal_in(reason: wire::Dismissal) -> Dismissal {
    match reason {
        wire::Dismissal::Backchannel => Dismissal::Backchannel,
        wire::Dismissal::Noise => Dismissal::Noise,
        wire::Dismissal::Empty => Dismissal::Empty,
        wire::Dismissal::Closed => Dismissal::Closed,
    }
}

const fn notice_out(kind: NoticeKind) -> wire::NoticeKind {
    match kind {
        NoticeKind::Recovered => wire::NoticeKind::Recovered,
        NoticeKind::Handoff => wire::NoticeKind::Handoff,
    }
}

const fn notice_in(kind: wire::NoticeKind) -> NoticeKind {
    match kind {
        wire::NoticeKind::Recovered => NoticeKind::Recovered,
        wire::NoticeKind::Handoff => NoticeKind::Handoff,
    }
}

const fn outcome_out(outcome: ToolOutcome) -> wire::ToolOutcome {
    match outcome {
        ToolOutcome::Completed => wire::ToolOutcome::Completed,
        ToolOutcome::Failed => wire::ToolOutcome::Failed,
        ToolOutcome::Cancelled => wire::ToolOutcome::Cancelled,
        ToolOutcome::InDoubt => wire::ToolOutcome::InDoubt,
    }
}

const fn outcome_in(outcome: wire::ToolOutcome) -> ToolOutcome {
    match outcome {
        wire::ToolOutcome::Completed => ToolOutcome::Completed,
        wire::ToolOutcome::Failed => ToolOutcome::Failed,
        wire::ToolOutcome::Cancelled => ToolOutcome::Cancelled,
        wire::ToolOutcome::InDoubt => ToolOutcome::InDoubt,
    }
}

#[cfg(test)]
mod tests {
    use waveport_domain::event::CloseReason;
    use waveport_domain::fact::{
        Dismissal, EpochCause, Fact, Lifecycle, NoticeKind, Stamped, ToolOutcome,
    };
    use waveport_domain::ids::{
        AgentId, BatchId, ContextVersion, Epoch, GenId, ParticipantId, Seq, ToolCallId, ToolId,
        TurnId,
    };
    use waveport_domain::text::TextArena;
    use waveport_domain::time::{MediaSpan, MediaTime};

    use waveport_domain::config::KernelConfig;
    use waveport_domain::kernel::{CallState, CallView, ItemView, Kernel, Snapshot, SnapshotParts};
    use waveport_domain::policy::DefaultPolicies;

    use super::{CodecError, decode, decode_state, encode, encode_state};

    /// Text that an encoding could get wrong.
    const AWKWARD: &str = "She said \"wait\" \u{2014} then:\n\t¿qué hora es? 🙂 \\ \u{0}";

    /// One fact of every kind, and some kinds more than once.
    #[expect(clippy::too_many_lines, reason = "one fact of every kind, at least")]
    fn every_kind(text: &mut TextArena) -> Vec<Fact> {
        let who = ParticipantId(3);
        let call = ToolCallId(u64::MAX);
        let generation = GenId(9);
        let agent = AgentId(1);
        vec![
            Fact::ParticipantJoined { who },
            Fact::ParticipantLeft { who },
            Fact::LifecycleChanged {
                to: Lifecycle::Draining,
                reason: None,
            },
            Fact::LifecycleChanged {
                to: Lifecycle::Closing,
                reason: Some(CloseReason::Abandoned),
            },
            Fact::LifecycleChanged {
                to: Lifecycle::Closed,
                reason: Some(CloseReason::Failure),
            },
            Fact::LifecycleChanged {
                to: Lifecycle::Running,
                reason: Some(CloseReason::Requested),
            },
            Fact::EpochAdvanced {
                to: Epoch(u32::MAX),
                cause: EpochCause::BargeIn,
            },
            Fact::EpochAdvanced {
                to: Epoch(2),
                cause: EpochCause::Interrupted,
            },
            Fact::EpochAdvanced {
                to: Epoch(3),
                cause: EpochCause::Handoff,
            },
            Fact::EpochAdvanced {
                to: Epoch(4),
                cause: EpochCause::Superseded,
            },
            Fact::EpochAdvanced {
                to: Epoch(5),
                cause: EpochCause::Failure,
            },
            Fact::EpochAdvanced {
                to: Epoch(6),
                cause: EpochCause::Recovery,
            },
            Fact::EpochAdvanced {
                to: Epoch(7),
                cause: EpochCause::Closing,
            },
            Fact::TurnCommitted {
                turn: TurnId(4),
                who,
                span: MediaSpan::new(
                    MediaTime::from_ticks(48_000),
                    MediaTime::from_ticks(u64::MAX),
                ),
                text: text.push(AWKWARD),
                addressed: true,
            },
            Fact::TurnCommitted {
                turn: TurnId(5),
                who,
                span: MediaSpan::new(MediaTime::from_ticks(7), MediaTime::from_ticks(7)),
                text: text.push(""),
                addressed: false,
            },
            Fact::TurnDismissed {
                turn: TurnId(6),
                who,
                reason: Dismissal::Backchannel,
            },
            Fact::TurnDismissed {
                turn: TurnId(7),
                who,
                reason: Dismissal::Noise,
            },
            Fact::TurnDismissed {
                turn: TurnId(8),
                who,
                reason: Dismissal::Empty,
            },
            Fact::TurnDismissed {
                turn: TurnId(9),
                who,
                reason: Dismissal::Closed,
            },
            Fact::NoticeAppended {
                kind: NoticeKind::Recovered,
                text: text.push("Some work was lost."),
            },
            Fact::NoticeAppended {
                kind: NoticeKind::Handoff,
                text: text.push(""),
            },
            Fact::UtteranceProgress {
                generation,
                agent,
                heard: text.push("It is a quarter "),
            },
            Fact::UtteranceCommitted {
                generation,
                agent,
                answers: Some(ContextVersion(12)),
                text: text.push("It is a quarter past nine."),
                truncated: false,
            },
            Fact::UtteranceCommitted {
                generation,
                agent,
                answers: None,
                text: text.push("It is a"),
                truncated: true,
            },
            Fact::ToolProposed {
                call,
                batch: BatchId(2),
                generation,
                tool: ToolId(u16::MAX),
                args: text.push("{\"zone\":\"local\"}"),
            },
            Fact::ToolAuthorized { call },
            Fact::ToolDispatched { call, attempt: 2 },
            Fact::ToolFinished {
                call,
                outcome: ToolOutcome::Completed,
                result: text.push("09:15"),
            },
            Fact::ToolFinished {
                call,
                outcome: ToolOutcome::Failed,
                result: text.push("no such zone"),
            },
            Fact::ToolFinished {
                call,
                outcome: ToolOutcome::Cancelled,
                result: text.push(""),
            },
            Fact::ToolFinished {
                call,
                outcome: ToolOutcome::InDoubt,
                result: text.push("unknown"),
            },
            Fact::BatchResolved { batch: BatchId(2) },
            Fact::HandoffRequested { to: AgentId(2) },
            Fact::AgentActivated { agent: AgentId(2) },
        ]
    }

    /// A number for each kind of fact. A new kind does not compile until
    /// it is given one, and the test below then fails until `every_kind`
    /// has it.
    const fn kind(fact: &Fact) -> usize {
        match fact {
            Fact::ParticipantJoined { .. } => 0,
            Fact::ParticipantLeft { .. } => 1,
            Fact::LifecycleChanged { .. } => 2,
            Fact::EpochAdvanced { .. } => 3,
            Fact::TurnCommitted { .. } => 4,
            Fact::TurnDismissed { .. } => 5,
            Fact::NoticeAppended { .. } => 6,
            Fact::UtteranceProgress { .. } => 7,
            Fact::UtteranceCommitted { .. } => 8,
            Fact::ToolProposed { .. } => 9,
            Fact::ToolAuthorized { .. } => 10,
            Fact::ToolDispatched { .. } => 11,
            Fact::ToolFinished { .. } => 12,
            Fact::BatchResolved { .. } => 13,
            Fact::HandoffRequested { .. } => 14,
            Fact::AgentActivated { .. } => 15,
        }
    }
    const KINDS: usize = 16;

    #[test]
    fn every_kind_of_fact_comes_back_as_it_was_written() {
        let mut text = TextArena::default();
        let facts = every_kind(&mut text);
        let mut seen = [false; KINDS];
        for (n, fact) in facts.iter().enumerate() {
            seen[kind(fact)] = true;
            let stamped = Stamped {
                seq: Seq(u64::try_from(n).unwrap() + 1),
                fact: *fact,
            };
            let record = encode(&stamped, &text).unwrap();

            let mut read = TextArena::default();
            // Not the first thing in its arena, so that a reference that
            // is not moved shows.
            let _ = read.push("something read earlier");
            let back = decode(&record, &mut read).unwrap();
            assert_eq!(back.seq, stamped.seq);
            assert_eq!(kind(&back.fact), kind(fact));
            assert_eq!(
                back.fact.text().map(|at| read.get(at)),
                fact.text().map(|at| text.get(at)),
                "{fact:?}"
            );
            // Apart from where its text is, it is the same fact.
            let moved = fact.text().map_or(back.fact, |at| back.fact.with_text(at));
            assert_eq!(moved, *fact);
        }
        assert_eq!(seen, [true; KINDS], "a kind of fact is not tried");
    }

    /// What a session holds after each of `facts` in turn, the first
    /// being what it holds before any.
    fn stages(facts: &[Fact], text: &TextArena) -> Vec<Snapshot> {
        let config = KernelConfig::default();
        let blank = Kernel::new(config.clone(), DefaultPolicies::default())
            .unwrap()
            .snapshot();
        let stamped: Vec<Stamped> = facts
            .iter()
            .zip(1..)
            .map(|(fact, seq)| Stamped {
                seq: Seq(seq),
                fact: *fact,
            })
            .collect();
        (0..=stamped.len())
            .map(|upto| {
                Kernel::restore(
                    config.clone(),
                    DefaultPolicies::default(),
                    &blank,
                    &stamped[..upto],
                    text,
                )
                .unwrap()
                .snapshot()
            })
            .collect()
    }

    #[test]
    fn a_state_comes_back_as_the_snapshot_it_was_written_from() {
        let mut text = TextArena::default();
        let facts = every_kind(&mut text);
        let stages = stages(&facts, &text);
        assert_eq!(stages.len(), facts.len() + 1);
        let mut heard = MediaTime::from_ticks(0);
        for (upto, snapshot) in stages.iter().enumerate() {
            heard = MediaTime::from_ticks(heard.ticks() + 48_000);
            let state = encode_state(snapshot, heard).unwrap();
            // Whatever stores a state may keep it as a line, as a record.
            let line = core::str::from_utf8(&state).unwrap();
            assert!(!line.contains('\n') && !line.contains('\r'), "{line}");
            assert!(
                line.starts_with(&format!("{{\"v\":1,\"upto\":{upto},")),
                "{line}"
            );

            let (back, heard_back) = decode_state(&state).unwrap();
            assert_eq!(&back, snapshot, "after fact {upto}");
            assert_eq!(heard_back, heard);
        }
        // Between them the stages hold something of everything a
        // snapshot can hold.
        let some = |holds: fn(&SnapshotParts<'_>) -> bool| {
            stages.iter().any(|snapshot| holds(&snapshot.parts()))
        };
        assert!(some(|parts| !parts.calls.is_empty()));
        assert!(some(|parts| !parts.batches.is_empty()));
        assert!(some(|parts| parts.speech.is_some()));
        assert!(some(|parts| parts.owed.is_some()));
        assert!(some(|parts| parts.handoff.is_some()));
        assert!(some(|parts| !parts.participants.is_empty()));
        assert!(some(|parts| parts.items.len() >= 5));
    }

    #[test]
    fn a_state_of_a_later_version_is_refused_by_its_version() {
        // A later version, with something this build has never heard of
        // where this build needs something else.
        let later = br#"{"v":2,"upto":4,"mood":"cheerful"}"#;
        assert_eq!(
            decode_state(later).unwrap_err(),
            CodecError::Newer { version: 2 }
        );
    }

    #[test]
    fn what_is_not_a_state_is_refused() {
        let mut text = TextArena::default();
        let facts = every_kind(&mut text);
        // A session with a call that was sent and a note in its
        // conversation.
        let stages = stages(&facts, &text);
        let busy = stages
            .iter()
            .find(|snapshot| {
                let parts = snapshot.parts();
                let sent = |call: &CallView| call.state == CallState::Dispatched;
                let note = |item: &ItemView<'_>| matches!(item, ItemView::Notice { .. });
                parts.calls.iter().any(sent) && parts.items.iter().any(note)
            })
            .unwrap();
        let whole = encode_state(busy, MediaTime::from_ticks(9)).unwrap();
        let whole = core::str::from_utf8(&whole).unwrap();
        let cases = [
            String::new(),
            "not json".to_owned(),
            // No version, and a record where a state belongs.
            whole.replacen("\"v\":1,", "", 1),
            r#"{"v":1,"seq":4,"fact":{"kind":"tool_authorized","call":9}}"#.to_owned(),
            // Cut short.
            whole[..whole.len() / 2].to_owned(),
            // An item of a kind nobody knows.
            whole.replacen("\"kind\":\"notice\"", "\"kind\":\"aside\"", 1),
            // A call in a state a journal never holds.
            whole.replacen("\"state\":\"dispatched\"", "\"state\":\"cancelling\"", 1),
        ];
        for case in cases {
            assert_ne!(case, whole, "the case changes nothing");
            let refused = decode_state(case.as_bytes());
            assert!(
                matches!(refused, Err(CodecError::Malformed(_))),
                "{case}: {refused:?}"
            );
        }
    }

    #[test]
    fn a_record_is_one_line_of_text() {
        // Whatever stores records may keep them a line each.
        let mut text = TextArena::default();
        for fact in every_kind(&mut text) {
            let record = encode(&Stamped { seq: Seq(1), fact }, &text).unwrap();
            let line = core::str::from_utf8(&record).unwrap();
            assert!(!line.contains('\n') && !line.contains('\r'), "{line}");
        }
    }

    #[test]
    fn a_record_of_a_later_version_is_refused_by_its_version() {
        let mut text = TextArena::default();
        // A later version, with a fact this build has never heard of.
        let later = br#"{"v":2,"seq":4,"fact":{"kind":"mood_changed","to":"cheerful"}}"#;
        assert_eq!(
            decode(later, &mut text),
            Err(CodecError::Newer { version: 2 })
        );
        assert!(text.is_empty(), "nothing was read");
    }

    #[test]
    fn what_is_not_a_record_is_refused() {
        let mut text = TextArena::default();
        let cases: [&[u8]; 6] = [
            b"",
            b"not json",
            br#"{"seq":4,"fact":{"kind":"tool_authorized","call":9}}"#,
            br#"{"v":1,"seq":4}"#,
            br#"{"v":1,"seq":4,"fact":{"kind":"mood_changed"}}"#,
            // A turn that ends before it begins.
            br#"{"v":1,"seq":4,"fact":{"kind":"turn_committed","turn":1,"who":1,"span":[9,3],"text":"hi","addressed":true}}"#,
        ];
        for case in cases {
            let refused = decode(case, &mut text);
            assert!(
                matches!(refused, Err(CodecError::Malformed(_))),
                "{}: {refused:?}",
                String::from_utf8_lossy(case)
            );
        }
        assert!(text.is_empty(), "nothing was read");
    }
}
