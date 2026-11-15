//! Kernel facts as journal records, and back.
//!
//! The kernel knows nothing of how a fact is stored, and the record format
//! in `waveport-protocol` knows nothing of the kernel. This module is where
//! the two meet. Every `match` here is exhaustive on purpose: a fact added
//! to the kernel does not compile until it has a record, and a record
//! added to the format does not compile until it has a fact.

use core::fmt;
use std::borrow::Cow;

use bytes::Bytes;
use waveport_domain::event::CloseReason;
use waveport_domain::fact::{
    Dismissal, EpochCause, Fact, Lifecycle, NoticeKind, Stamped, ToolOutcome,
};
use waveport_domain::ids::{
    AgentId, BatchId, ContextVersion, Epoch, GenId, ParticipantId, Seq, ToolCallId, ToolId, TurnId,
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
    // The version is looked at first, and alone: a record of a later
    // version may hold facts this build cannot read at all.
    #[derive(serde::Deserialize)]
    struct Versioned {
        v: u32,
    }
    let malformed = |error: serde_json::Error| CodecError::Malformed(error.to_string());
    let Versioned { v } = serde_json::from_slice(record).map_err(malformed)?;
    if v > wire::VERSION {
        return Err(CodecError::Newer { version: v });
    }
    let record: wire::Record<'_> = serde_json::from_slice(record).map_err(malformed)?;
    Ok(Stamped {
        seq: Seq(record.seq),
        fact: from_wire(&record.fact, text)?,
    })
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

    use super::{CodecError, decode, encode};

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
