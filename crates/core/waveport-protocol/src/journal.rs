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
//! A journal also holds a session's [`State`] as of one of its facts: what
//! the facts up to there add up to, stored so that the session can be
//! rebuilt from there and the records before it forgotten. The same rules
//! hold for it, and it says its version as a record does.
//!
//! The types here hold plain numbers and text. What the numbers mean is
//! the session kernel's business, and this crate does not depend on it:
//! the runtime turns a kernel fact into a record and back, and a kernel
//! snapshot into a state and back.

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

/// A session's state as of one of its facts: everything the facts up to
/// that one add up to, and nothing that was only under way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State<'a> {
    /// The version of the format the state was written in.
    pub v: u32,
    /// The number of the last fact the state includes.
    pub upto: u64,
    /// Where the last audio ends that a turn up to there was spoken in,
    /// in ticks of 1/48,000 s. A session that is rebuilt goes on after
    /// it.
    pub heard_until: u64,
    /// The epoch.
    pub epoch: u32,
    /// The stage of the session's life.
    pub lifecycle: Lifecycle,
    /// Why the session is closing, once it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_for: Option<CloseReason>,
    /// The agent that holds the floor.
    pub front: u8,
    /// The agent that was asked to take the floor and has not yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<u8>,
    /// The lowest turn number no fact has named.
    pub turn_floor: u32,
    /// The lowest response number no fact has named.
    pub gen_floor: u32,
    /// The number the next tool call gets.
    pub next_call: u64,
    /// The number the next batch of tool calls gets.
    pub next_batch: u32,
    /// The humans in the session.
    pub participants: Vec<u16>,
    /// The tool calls that have not ended.
    pub calls: Vec<OpenCall>,
    /// The batches of tool calls that are not resolved.
    pub batches: Vec<OpenBatch>,
    /// The conversation, oldest item first.
    #[serde(borrow)]
    pub items: Vec<Item<'a>>,
    /// The newest item of the conversation that is owed a response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owed: Option<Owed>,
    /// How many items the conversation had when it last had its
    /// response.
    pub answered: u32,
    /// The agent's speech that was being heard.
    #[serde(default, borrow, skip_serializing_if = "Option::is_none")]
    pub speech: Option<Speech<'a>>,
}

/// A tool call that has not ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenCall {
    /// The call.
    pub call: u64,
    /// Which tool, by its place in the session's configuration.
    pub tool: u16,
    /// The calls proposed together with it.
    pub batch: u32,
    /// The response that asked for it.
    pub by: u32,
    /// How far it has got.
    pub state: CallState,
    /// How many times it was sent before, after the session was
    /// restarted.
    pub attempt: u8,
}

/// How far a tool call that has not ended has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    /// Asked for by a response.
    Proposed,
    /// Cleared to run.
    Authorized,
    /// Sent to its tool.
    Dispatched,
}

/// A batch of tool calls that is not resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenBatch {
    /// The batch.
    pub batch: u32,
    /// The response that asked for its calls.
    pub by: u32,
    /// How many of its calls have not ended.
    pub pending: u32,
    /// Whether any of its calls was sent, or ended in another way than
    /// by being withdrawn.
    pub ran: bool,
}

/// One item of a conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item<'a> {
    /// What a user said.
    UserTurn {
        /// The turn.
        turn: u32,
        /// Who said it.
        who: u16,
        /// The words.
        #[serde(borrow)]
        text: Cow<'a, str>,
        /// Whether it was meant for the agent.
        addressed: bool,
    },
    /// What an agent said, as far as it was heard.
    Utterance {
        /// The response that was spoken.
        generation: u32,
        /// The agent.
        agent: u8,
        /// The words the listener heard.
        #[serde(borrow)]
        text: Cow<'a, str>,
        /// Whether the agent was cut off.
        truncated: bool,
    },
    /// A tool call an agent asked for.
    ToolCall {
        /// The call.
        call: u64,
        /// Which tool, by its place in the session's configuration.
        tool: u16,
        /// The response that asked.
        generation: u32,
        /// The arguments.
        #[serde(borrow)]
        args: Cow<'a, str>,
    },
    /// How a tool call ended.
    ToolResult {
        /// The call.
        call: u64,
        /// How it ended.
        outcome: ToolOutcome,
        /// What the language model is told.
        #[serde(borrow)]
        text: Cow<'a, str>,
    },
    /// A note to the language model.
    Notice {
        /// What the note is about.
        about: NoticeKind,
        /// The note.
        #[serde(borrow)]
        text: Cow<'a, str>,
    },
}

/// The item of a conversation that is owed a response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owed {
    /// How many items the conversation had when the item was its last.
    pub version: u32,
    /// What the response is for.
    pub cause: Cause,
}

/// What a response is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "for", rename_all = "snake_case")]
pub enum Cause {
    /// A user turn.
    Reply {
        /// The turn.
        turn: u32,
    },
    /// The results of the tool calls an earlier response asked for.
    Continuation {
        /// Those calls.
        batch: u32,
    },
    /// A tool call whose tool is another agent.
    Delegated {
        /// The call.
        call: u64,
    },
}

/// An agent's speech that was being heard, as far as facts had recorded
/// it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Speech<'a> {
    /// The response being spoken.
    pub generation: u32,
    /// Its agent.
    pub agent: u8,
    /// What had been recorded as heard of it.
    #[serde(borrow)]
    pub heard: Cow<'a, str>,
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

    use super::{
        CallState, Cause, CloseReason, Fact, Item, Lifecycle, NoticeKind, OpenBatch, OpenCall,
        Owed, Record, Speech, State, ToolOutcome, VERSION,
    };

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

    fn state() -> State<'static> {
        State {
            v: VERSION,
            upto: 17,
            heard_until: 96_000,
            epoch: 3,
            lifecycle: Lifecycle::Running,
            closed_for: None,
            front: 0,
            handoff: None,
            turn_floor: 5,
            gen_floor: 7,
            next_call: 3,
            next_batch: 2,
            participants: vec![1],
            calls: vec![OpenCall {
                call: 2,
                tool: 0,
                batch: 1,
                by: 1,
                state: CallState::Dispatched,
                attempt: 1,
            }],
            batches: vec![OpenBatch {
                batch: 1,
                by: 1,
                pending: 1,
                ran: true,
            }],
            items: vec![
                Item::UserTurn {
                    turn: 1,
                    who: 1,
                    text: Cow::Borrowed("what time is it"),
                    addressed: true,
                },
                Item::ToolCall {
                    call: 1,
                    tool: 0,
                    generation: 1,
                    args: Cow::Borrowed("{}"),
                },
                Item::ToolResult {
                    call: 1,
                    outcome: ToolOutcome::Completed,
                    text: Cow::Borrowed("ten past three"),
                },
                Item::Utterance {
                    generation: 2,
                    agent: 0,
                    text: Cow::Borrowed("It is ten"),
                    truncated: true,
                },
                Item::Notice {
                    about: NoticeKind::Recovered,
                    text: Cow::Borrowed("Some work was lost."),
                },
            ],
            owed: Some(Owed {
                version: 1,
                cause: Cause::Reply { turn: 1 },
            }),
            answered: 1,
            speech: Some(Speech {
                generation: 6,
                agent: 0,
                heard: Cow::Borrowed("Goodbye, and "),
            }),
        }
    }

    #[test]
    fn a_state_is_written_as_it_is_documented() {
        // As for a record: a change here is a change to what builds that
        // are not written yet were promised.
        let written = serde_json::to_string(&state()).unwrap();
        assert_eq!(
            written,
            concat!(
                r#"{"v":1,"upto":17,"heard_until":96000,"epoch":3,"lifecycle":"running","front":0,"#,
                r#""turn_floor":5,"gen_floor":7,"next_call":3,"next_batch":2,"participants":[1],"#,
                r#""calls":[{"call":2,"tool":0,"batch":1,"by":1,"state":"dispatched","attempt":1}],"#,
                r#""batches":[{"batch":1,"by":1,"pending":1,"ran":true}],"#,
                r#""items":[{"kind":"user_turn","turn":1,"who":1,"text":"what time is it","addressed":true},"#,
                r#"{"kind":"tool_call","call":1,"tool":0,"generation":1,"args":"{}"},"#,
                r#"{"kind":"tool_result","call":1,"outcome":"completed","text":"ten past three"},"#,
                r#"{"kind":"utterance","generation":2,"agent":0,"text":"It is ten","truncated":true},"#,
                r#"{"kind":"notice","about":"recovered","text":"Some work was lost."}],"#,
                r#""owed":{"version":1,"cause":{"for":"reply","turn":1}},"answered":1,"#,
                r#""speech":{"generation":6,"agent":0,"heard":"Goodbye, and "}}"#,
            )
        );
        let read: State<'_> = serde_json::from_str(&written).unwrap();
        assert_eq!(read, state());
        // The text is borrowed from what the state was read from.
        let Some(Item::UserTurn { text, .. }) = read.items.first() else {
            panic!("another item");
        };
        assert!(matches!(text, Cow::Borrowed("what time is it")));

        // What a response can be for.
        let causes = [
            (
                Cause::Continuation { batch: 4 },
                r#"{"for":"continuation","batch":4}"#,
            ),
            (
                Cause::Delegated { call: 9 },
                r#"{"for":"delegated","call":9}"#,
            ),
        ];
        for (cause, expected) in causes {
            assert_eq!(serde_json::to_string(&cause).unwrap(), expected);
        }
    }

    #[test]
    fn a_state_leaves_out_what_is_not_so() {
        let quiet = State {
            closed_for: Some(CloseReason::Requested),
            handoff: Some(1),
            calls: Vec::new(),
            batches: Vec::new(),
            items: Vec::new(),
            owed: None,
            speech: None,
            ..state()
        };
        let written = serde_json::to_string(&quiet).unwrap();
        assert!(written.contains(r#""closed_for":"requested","front":0,"handoff":1,"#));
        assert!(!written.contains("owed") && !written.contains("speech"));
        assert_eq!(serde_json::from_str::<State<'_>>(&written).unwrap(), quiet);
    }

    #[test]
    fn a_state_from_a_later_build_is_read_as_far_as_it_is_known() {
        // A field this build does not know is skipped, in the state and
        // in what it holds.
        let later = serde_json::to_string(&state())
            .unwrap()
            .replacen(r#""epoch":3"#, r#""epoch":3,"region":"eu-2""#, 1)
            .replacen(r#""ran":true"#, r#""ran":true,"budget":4"#, 1);
        assert_eq!(serde_json::from_str::<State<'_>>(&later).unwrap(), state());
        // An item of a kind this build does not know is refused: a
        // conversation with a hole in it is another conversation.
        let unknown = serde_json::to_string(&state()).unwrap().replacen(
            r#""kind":"notice""#,
            r#""kind":"aside""#,
            1,
        );
        assert!(serde_json::from_str::<State<'_>>(&unknown).is_err());
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
