//! The records of a session's journal.
//!
//! A record is one fact about a session: something that became true of it,
//! with its number. A session is rebuilt from its records, so a record
//! written by one build has to be read by the builds that follow it, and,
//! when a deployment is rolled back, by the one before.
//!
//! The rules that keep that possible:
//!
//! - A record says which version of the format it was written in
//!   ([`VERSION`]). A reader refuses a version above its own.
//! - A field may be added to a fact without a new version, if a reader
//!   that does not know it can leave it out. Readers skip fields they do
//!   not know.
//! - Anything else is a new version: a new kind of fact, a field without
//!   which a fact means something else, a field whose meaning changes.
//!
//! The types here hold plain numbers and text. What the numbers mean is
//! the session kernel's business, and this crate does not depend on it:
//! the runtime turns a kernel fact into a record and back.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

/// The version of the format this build writes.
pub const VERSION: u32 = 1;

/// One record: a fact and where it stands in the session's journal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record<'a> {
    /// The version of the format the record was written in.
    pub v: u32,
    /// The number of the fact. A session's facts are numbered from 1
    /// without gaps.
    pub seq: u64,
    /// What became true.
    #[serde(borrow)]
    pub fact: Fact<'a>,
}

/// Something that became true of a session.
///
/// Participants, agents, turns, responses, tool calls, batches and tools
/// are named by their numbers. Text is borrowed from what the record was
/// read from, where it can be.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fact<'a> {
    /// A human joined.
    ParticipantJoined {
        /// Who.
        who: u16,
    },
    /// A human left.
    ParticipantLeft {
        /// Who.
        who: u16,
    },
    /// The session moved to another stage of its life.
    LifecycleChanged {
        /// The new stage.
        to: Lifecycle,
        /// Why the session is closing, once it is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<CloseReason>,
    },
    /// Agent output that held the floor became invalid.
    EpochAdvanced {
        /// The new epoch.
        to: u32,
        /// What invalidated the output.
        cause: EpochCause,
    },
    /// A user turn ended and entered the conversation.
    TurnCommitted {
        /// The turn.
        turn: u32,
        /// Its speaker.
        who: u16,
        /// The audio it was spoken in: where it begins and where it ends,
        /// in ticks of 1/48,000 s. The two are equal for typed text.
        span: [u64; 2],
        /// What was said.
        #[serde(borrow)]
        text: Cow<'a, str>,
        /// Whether it was meant for the agent.
        addressed: bool,
    },
    /// A user turn ended without entering the conversation.
    TurnDismissed {
        /// The turn.
        turn: u32,
        /// Its speaker.
        who: u16,
        /// Why.
        reason: Dismissal,
    },
    /// A note to the language model entered the conversation.
    NoticeAppended {
        /// What the note is about.
        about: NoticeKind,
        /// The note.
        #[serde(borrow)]
        text: Cow<'a, str>,
    },
    /// More of an agent's speech was heard.
    UtteranceProgress {
        /// The response being spoken.
        generation: u32,
        /// Its agent.
        agent: u8,
        /// The text heard since the previous record for this response.
        #[serde(borrow)]
        heard: Cow<'a, str>,
    },
    /// An agent's speech ended and entered the conversation.
    UtteranceCommitted {
        /// The response that was spoken.
        generation: u32,
        /// Its agent.
        agent: u8,
        /// The version of the conversation this speech answers, if it
        /// answers any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answers: Option<u32>,
        /// What the listener heard.
        #[serde(borrow)]
        text: Cow<'a, str>,
        /// Whether the agent was cut off.
        truncated: bool,
    },
    /// A response asked for a tool call.
    ToolProposed {
        /// The call.
        call: u64,
        /// The calls proposed together with it.
        batch: u32,
        /// The response that asked.
        generation: u32,
        /// Which tool, by its place in the session's configuration.
        tool: u16,
        /// Its arguments.
        #[serde(borrow)]
        args: Cow<'a, str>,
    },
    /// A call may be dispatched.
    ToolAuthorized {
        /// The call.
        call: u64,
    },
    /// A call was dispatched.
    ToolDispatched {
        /// The call.
        call: u64,
        /// 0 for the first dispatch.
        attempt: u8,
    },
    /// A call ended.
    ToolFinished {
        /// The call.
        call: u64,
        /// How it ended.
        outcome: ToolOutcome,
        /// What the language model is told.
        #[serde(borrow)]
        result: Cow<'a, str>,
    },
    /// Every call of a batch has ended.
    BatchResolved {
        /// The batch.
        batch: u32,
    },
    /// A handoff to another agent was asked for.
    HandoffRequested {
        /// The agent to activate.
        to: u8,
    },
    /// An agent took the floor.
    AgentActivated {
        /// The agent.
        agent: u8,
    },
}

/// The stages of a session's life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    /// Conversing.
    Running,
    /// Finishing what is under way and starting nothing new.
    Draining,
    /// Stopping everything that has not finished.
    Closing,
    /// Over.
    Closed,
}

/// Why a session closes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    /// The application or a participant asked for it.
    Requested,
    /// Everyone left.
    Abandoned,
    /// The session could not go on.
    Failure,
}

/// What made agent output invalid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpochCause {
    /// A user spoke over the agent and wanted the floor.
    BargeIn,
    /// The application interrupted the agent.
    Interrupted,
    /// Another agent took the floor.
    Handoff,
    /// A newer turn made the response out of date before any of it was
    /// heard.
    Superseded,
    /// A part that was producing the output failed or stalled.
    Failure,
    /// The session was rebuilt after a crash.
    Recovery,
    /// The session is closing.
    Closing,
}

/// Why a user turn did not enter the conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dismissal {
    /// It was a listener's noise of agreement.
    Backchannel,
    /// It was not speech meant for anyone.
    Noise,
    /// No words were recognised in it.
    Empty,
    /// The session closed before it ended.
    Closed,
}

/// What a note to the language model is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    /// The session was rebuilt after a crash, and some work was lost.
    Recovered,
    /// Another agent is taking over, and this is what it is told.
    Handoff,
}

/// How a tool call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    /// The tool returned a result.
    Completed,
    /// The tool failed, or was never run.
    Failed,
    /// The call was cancelled and left no effect.
    Cancelled,
    /// The call was dispatched, and whether it took effect is unknown.
    InDoubt,
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::{CloseReason, Fact, Lifecycle, Record, VERSION};

    fn written(record: &Record<'_>) -> String {
        serde_json::to_string(record).unwrap()
    }

    #[test]
    fn a_record_is_written_as_it_is_documented() {
        // The format is a promise to builds that are not written yet. A
        // change here is a change to that promise.
        let turn = Record {
            v: VERSION,
            seq: 7,
            fact: Fact::TurnCommitted {
                turn: 2,
                who: 1,
                span: [48_000, 105_600],
                text: Cow::Borrowed("what time is it"),
                addressed: true,
            },
        };
        assert_eq!(
            written(&turn),
            r#"{"v":1,"seq":7,"fact":{"kind":"turn_committed","turn":2,"who":1,"span":[48000,105600],"text":"what time is it","addressed":true}}"#
        );

        let running = Record {
            v: VERSION,
            seq: 1,
            fact: Fact::LifecycleChanged {
                to: Lifecycle::Draining,
                reason: None,
            },
        };
        assert_eq!(
            written(&running),
            r#"{"v":1,"seq":1,"fact":{"kind":"lifecycle_changed","to":"draining"}}"#
        );
        let closing = Record {
            v: VERSION,
            seq: 2,
            fact: Fact::LifecycleChanged {
                to: Lifecycle::Closing,
                reason: Some(CloseReason::Abandoned),
            },
        };
        assert_eq!(
            written(&closing),
            r#"{"v":1,"seq":2,"fact":{"kind":"lifecycle_changed","to":"closing","reason":"abandoned"}}"#
        );
    }

    #[test]
    fn text_that_needs_no_unescaping_is_borrowed() {
        let plain = r#"{"v":1,"seq":3,"fact":{"kind":"notice_appended","about":"recovered","text":"Some work was lost."}}"#;
        let record: Record<'_> = serde_json::from_str(plain).unwrap();
        let Fact::NoticeAppended { text, .. } = record.fact else {
            panic!("another fact");
        };
        assert!(matches!(text, Cow::Borrowed("Some work was lost.")));

        let escaped = r#"{"v":1,"seq":3,"fact":{"kind":"notice_appended","about":"recovered","text":"He said \"no\".\n"}}"#;
        let record: Record<'_> = serde_json::from_str(escaped).unwrap();
        let Fact::NoticeAppended { text, .. } = record.fact else {
            panic!("another fact");
        };
        assert_eq!(text, "He said \"no\".\n");
    }

    #[test]
    fn a_field_this_build_does_not_know_is_skipped() {
        // Written by a later build that added a field to a fact and to
        // the record.
        let later = r#"{"v":1,"seq":4,"node":"eu-2","fact":{"kind":"tool_authorized","call":9,"by":"policy"}}"#;
        let record: Record<'_> = serde_json::from_str(later).unwrap();
        assert_eq!(record.seq, 4);
        assert_eq!(record.fact, Fact::ToolAuthorized { call: 9 });
    }

    #[test]
    fn a_fact_this_build_does_not_know_is_refused() {
        let unknown = r#"{"v":1,"seq":4,"fact":{"kind":"mood_changed","to":"cheerful"}}"#;
        assert!(serde_json::from_str::<Record<'_>>(unknown).is_err());
        // And so is a number that does not fit.
        let wide = r#"{"v":1,"seq":4,"fact":{"kind":"agent_activated","agent":300}}"#;
        assert!(serde_json::from_str::<Record<'_>>(wide).is_err());
    }
}
