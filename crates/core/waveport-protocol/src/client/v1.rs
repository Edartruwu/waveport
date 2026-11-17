//! Client protocol, version 1.
//!
//! What a client and a session say to each other: a browser, a telephone
//! bridge or a program on one side, and the server that runs the session
//! on the other. The messages are the same on every connection that can
//! carry them: a WebSocket, the data channel of a WebRTC call, and, for
//! what the server says, a stream of server-sent events.
//!
//! # Messages
//!
//! Everything that is not audio is one JSON object whose `type` says
//! what it is. [`FromClient`] is what a client says and [`FromServer`]
//! what a server says. On a WebSocket each is a text message. Audio is a
//! binary message that holds audio and nothing else: plain samples of 16
//! bits, little end first, or one Opus packet, as [`AudioFormat`] says.
//! On a WebRTC call the audio is the call's, and only the JSON is here.
//!
//! # A connection
//!
//! The client speaks first, with [`FromClient::Hello`], and is answered
//! with [`FromServer::Welcome`], or with an error and a goodbye. A hello
//! that names no audio asks for a session of text. After the welcome
//! each side says what it has to say, in any order.
//!
//! Audio from the server is in runs. A [`FromServer::Mark`] says that
//! the audio which follows is that of one response, and another that the
//! response has been sent whole. The client says how much of a run it
//! has played ([`FromClient::Played`]), which is how the session knows
//! what its listener heard. [`FromServer::Clear`] tells the client to
//! discard what it holds of a run and has not played.
//!
//! # Stopping, dropping and leaving
//!
//! These are three things. [`FromClient::Interrupt`] stops the agent and
//! the session goes on. A connection that drops leaves the session
//! waiting, for as long as the welcome's `grace_ms` says, or for as
//! long as it takes where the welcome names no time: a client that
//! comes back says so in its hello ([`Resume`]) and is told what it
//! missed. [`FromClient::Bye`] ends the session.
//!
//! # What a client is told
//!
//! Two kinds of thing. A [`Fact`] is something that became true of the
//! session, with a number. Facts are numbered in the order they came
//! about, and the numbers a client sees rise but need not follow one
//! another: some facts are the server's alone. [`News`] is what is going
//! on and is not kept: who is speaking, what the agent is doing, words
//! as they go out and words as they are recognised. News that was missed
//! is not told again, because what came of it is in the facts.
//!
//! A fact here is a client's view of a record of the session's journal
//! ([`Fact::shown`]). A client is told of tool calls only if it asks to
//! be, and the arguments of a call and what its tool answered do not
//! leave the server unless the server is told that they may ([`Show`]).
//!
//! # Coming back
//!
//! A client that comes back says the number of the last fact it was
//! sent, and is sent the facts after it. Two things can be in the way.
//! The client was away for longer than the server holds facts. Or the
//! process that ran the session died: a fact is told when it comes
//! about, which is before it is stored, so the session that was rebuilt
//! from what was stored may never have done the last things the client
//! was told, and gives their numbers to others. The numbers alone do
//! not show that, so a session says which of its lives it is in
//! (`life` in the welcome), and a client that comes back says which one
//! it was told of. Either way the client is sent [`FromServer::Gap`],
//! then the session's [`State`], which takes the place of what the
//! client holds, then the facts after it.
//!
//! A [`View`] is what the facts add up to. Applying a state and the
//! facts after it gives the view that applying every fact would have.
//!
//! # Change
//!
//! A reader skips a field it does not know, and takes a message, a fact
//! or a piece of news of a kind it does not know as
//! [`Unknown`](FromServer::Unknown), so a field or a kind can be added
//! without a new version. Anything a reader would misread without
//! noticing is a new version, and a client names the version it speaks
//! in its hello.

use serde::{Deserialize, Serialize};

use crate::journal;

/// The version of the protocol.
pub const VERSION: u32 = 1;

/// The name of the protocol, which a client gives as it connects where
/// the connection has a place for one: the subprotocol of a WebSocket,
/// the label of a data channel.
pub const NAME: &str = "waveport.client.v1";

/// What a client token begins with. A client token lets its holder
/// connect to one session, once, and is no key to anything else.
pub const CLIENT_TOKEN: &str = "wpc1_";

/// What a resume token begins with. A resume token lets its holder go on
/// with one session after its connection dropped, or after the process
/// that ran it died.
pub const RESUME_TOKEN: &str = "wpr1_";

/// The rates audio can have, in samples a second.
pub const RATES: [u32; 4] = [8_000, 16_000, 24_000, 48_000];

/// How many keys one `dtmf` message may hold.
pub const MOST_KEYS: usize = 32;

/// How many bytes of text one `text` message may hold.
pub const MOST_TEXT: usize = 16 * 1024;

/// The keys of a telephone, as they are written.
pub const KEYS: &str = "0123456789*#ABCD";

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde gives a reference to the field"
)]
const fn is_false(flag: &bool) -> bool {
    !*flag
}

/// How audio is written on a connection that carries it as messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    /// Plain samples of 16 bits, little end first, one channel. A
    /// message holds any whole number of them.
    Pcm16,
    /// Opus, one channel. A message holds one packet.
    Opus,
}

/// The audio of a connection, the same in both directions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AudioFormat {
    /// How it is written.
    pub codec: Codec,
    /// Samples a second: one of [`RATES`].
    pub rate: u32,
}

/// What a client that comes back says of where it was.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    /// The resume token its welcome gave it.
    pub token: String,
    /// The life of the session its welcome named: the one the facts it
    /// was sent were of.
    pub life: u64,
    /// The number of the last fact it was sent, or 0 if it was sent
    /// none.
    pub seen: u64,
}

/// What a client says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromClient {
    /// The first thing a client says.
    Hello {
        /// The version of the protocol it speaks.
        v: u32,
        /// Its client token, where the server asks for one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        /// The audio it sends and wants. A client that names none asks
        /// for a session of text. On a WebRTC call the audio is the
        /// call's, and this is left out.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<AudioFormat>,
        /// What it wants to be told beyond the facts: any of
        /// [`features`]. A name the server does not know is passed over.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        features: Vec<String>,
        /// Where it was, if it comes back to a session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume: Option<Resume>,
    },
    /// How much of a run of audio the client has played so far.
    Played {
        /// The run, by the number its mark gave it.
        run: u32,
        /// How many of its samples have been played.
        samples: u32,
    },
    /// What a WebRTC call has played, as its browser can say it: where
    /// in the server's stream the audio is that was played last, or how
    /// much audio has come out, or both. A server that is told both goes
    /// by the first.
    PlayedRtp {
        /// The RTP time of the audio that was played last.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ts: Option<u32>,
        /// How many samples have come out of the buffer since the call
        /// began.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        emitted: Option<u64>,
        /// How many of those were made up for audio that did not arrive.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        concealed: Option<u64>,
        /// How long what came out takes to be heard, in milliseconds.
        #[serde(default)]
        delay_ms: u32,
    },
    /// Something typed in place of speech.
    Text {
        /// What was typed.
        text: String,
    },
    /// Keys of a telephone that were pressed, in order: characters of
    /// [`KEYS`].
    Dtmf {
        /// The keys.
        keys: String,
    },
    /// The agent is to stop speaking. The session goes on.
    Interrupt,
    /// The client is leaving, and the session is to end.
    Bye,
    /// A message of a kind this version does not have.
    #[serde(other)]
    Unknown,
}

/// What a client may ask to be told beyond the facts, by the names a
/// hello gives them.
pub mod features {
    /// The text of a response as it goes out: [`News::Said`](super::News::Said).
    pub const SAID: &str = "said";
    /// The words of a speaker as far as they are recognised:
    /// [`News::Hearing`](super::News::Hearing).
    pub const HEARING: &str = "hearing";
    /// The facts about tool calls.
    pub const TOOLS: &str = "tools";
    /// Every feature there is.
    pub const ALL: [&str; 3] = [SAID, HEARING, TOOLS];
}

/// Why something that was asked is not done, for a program. The control
/// API says it with the same codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The client speaks a version the server does not.
    Version,
    /// The token is missing, not one, spent, or for another session.
    Token,
    /// Whoever asks may not: the key is missing or wrong, or the request
    /// comes from where none is taken.
    Forbidden,
    /// The message is not one of the protocol, or says something a
    /// message of its kind cannot.
    Malformed,
    /// The message is of a kind the server does not know.
    UnknownMessage,
    /// The message comes at a place where it cannot: a second hello, or
    /// anything before the first.
    OutOfPlace,
    /// The session cannot do what is asked: audio for a session of text,
    /// a rate or a codec that is not offered.
    Unsupported,
    /// The session has too much waiting for it. The message was not
    /// taken, and can be sent again.
    Busy,
    /// There is no such session, or none to go on with.
    NoSession,
    /// There is no agent of that name.
    NoAgent,
    /// Something failed in the server.
    Internal,
    /// A code this version does not have.
    #[serde(other)]
    Unknown,
}

/// Why a server says goodbye.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ByeReason {
    /// The session is over. There is nothing to come back to.
    Ended,
    /// Another connection took the session. This one should not come
    /// back by itself.
    Replaced,
    /// The server is going away and the session is kept: the client can
    /// come back with its resume token.
    GoingAway,
    /// The client was refused, as the error before this says.
    Refused,
    /// A reason this version does not have.
    #[serde(other)]
    Unknown,
}

/// What a server says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromServer {
    /// The answer to a hello: the session is there.
    Welcome {
        /// The version of the protocol the server speaks from here on.
        v: u32,
        /// What the session is called.
        session: String,
        /// Which life of the session this is. The number goes up each
        /// time the session is rebuilt from what was stored of it, after
        /// its process died. What a client was told in an earlier life
        /// it cannot go on from: a gap and a state follow the welcome.
        life: u64,
        /// Whether the client came back to a session that was under way.
        resumed: bool,
        /// What lets the client come back, if the session is kept where
        /// that can be done.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_token: Option<String>,
        /// The audio of the connection, if it carries any as messages.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<AudioFormat>,
        /// Which of the features the client asked for it will be told.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        features: Vec<String>,
        /// The number the next fact gets that comes about in the
        /// session. A fact with a lower number that follows this welcome
        /// came about before it, and is sent because the client had not
        /// seen it.
        next_seq: u64,
        /// How long the session waits for a client whose connection
        /// dropped, in milliseconds, before it ends. Left out where a
        /// session does not end for that: one that is followed and
        /// written to, and is on no line.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grace_ms: Option<u32>,
    },
    /// Where the audio of one response begins, or that all of it has
    /// been sent.
    Mark {
        /// The number of the run. The audio that follows, up to the next
        /// mark, is its audio.
        run: u32,
        /// The response it is the audio of, by the number its facts and
        /// its news carry.
        generation: u32,
        /// Whether this is the end of the run and not its beginning.
        #[serde(default, skip_serializing_if = "is_false")]
        end: bool,
    },
    /// What the client holds of this run, and of every run before it,
    /// and has not played is to be discarded.
    Clear {
        /// The number of the run.
        run: u32,
    },
    /// Something became true of the session.
    Fact {
        /// Its number.
        seq: u64,
        /// What it is.
        #[serde(flatten)]
        fact: Fact,
    },
    /// Something is going on.
    Told(News),
    /// What the client holds cannot be gone on from: facts it missed are
    /// no longer held, or it was told of an earlier life of the session,
    /// which may have done what this one never did. A state follows,
    /// which takes the place of what the client holds, and then the
    /// facts from this number on.
    Gap {
        /// The number of the first fact that follows the state.
        resumes: u64,
    },
    /// What the facts up to one of them add up to.
    State(State),
    /// A message of the client was not taken, or the session failed.
    Error {
        /// Why, for a program.
        code: ErrorCode,
        /// Why, for a person. What it says is not part of the protocol.
        message: String,
    },
    /// The server is done with this connection, which closes after this.
    Bye {
        /// Why.
        reason: ByeReason,
    },
    /// A message of a kind this version does not have.
    #[serde(other)]
    Unknown,
}

/// The stages of a session's life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    /// Conversing.
    #[default]
    Running,
    /// Finishing what is under way and starting nothing new.
    Draining,
    /// Stopping everything that has not finished.
    Closing,
    /// Over.
    Closed,
    /// A stage this version does not have.
    #[serde(other)]
    Unknown,
}

/// Why a session closes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    /// The application or a participant asked for it.
    Requested,
    /// Everyone left, or the line dropped and stayed gone.
    Abandoned,
    /// The session could not go on.
    Failure,
    /// A reason this version does not have.
    #[serde(other)]
    Unknown,
}

/// Why what an agent was saying or preparing was cut off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cut {
    /// Somebody spoke over the agent and wanted the floor, or pressed a
    /// key.
    BargeIn,
    /// The application interrupted the agent, or the line dropped.
    Interrupted,
    /// Another agent took the floor.
    Handoff,
    /// A newer turn made the response out of date before any of it was
    /// heard.
    Superseded,
    /// A part that was producing the response failed or stalled.
    Failure,
    /// The session was rebuilt after its process died.
    Recovery,
    /// The session is closing.
    Closing,
    /// A cause this version does not have.
    #[serde(other)]
    Unknown,
}

/// Why something a participant did was not a turn.
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
    /// A reason this version does not have.
    #[serde(other)]
    Unknown,
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
    /// The call was sent, and whether it took effect is not known.
    InDoubt,
    /// An outcome this version does not have.
    #[serde(other)]
    Unknown,
}

/// What an agent is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    /// Nothing: it is the user's turn.
    Listening,
    /// Working on a response that has not begun to be heard.
    Thinking,
    /// Speaking, or writing in a session of text.
    Speaking,
    /// Something this version does not have.
    #[serde(other)]
    Unknown,
}

/// Something that became true of a session, as a client is shown it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fact {
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
    /// What an agent was saying or preparing was cut off. What was heard
    /// of it is the utterance that is committed next, if any of it was.
    CutOff {
        /// Why.
        cause: Cut,
    },
    /// A participant's turn ended and entered the conversation.
    TurnCommitted {
        /// The turn.
        turn: u32,
        /// Whose it is.
        who: u16,
        /// What was said, or typed.
        text: String,
        /// Whether it was meant for the agent.
        addressed: bool,
    },
    /// Keys of a telephone that a participant pressed one after the
    /// other entered the conversation.
    KeysCommitted {
        /// The turn.
        turn: u32,
        /// Who pressed them.
        who: u16,
        /// The keys, in the order they were pressed.
        keys: String,
    },
    /// Something a participant did was not a turn.
    TurnDismissed {
        /// The turn.
        turn: u32,
        /// Whose it is.
        who: u16,
        /// Why it was not one.
        reason: Dismissal,
    },
    /// More of an agent's response was heard.
    UtteranceProgress {
        /// The response.
        generation: u32,
        /// Its agent.
        agent: u8,
        /// The text heard since the fact before this one about the
        /// response.
        heard: String,
    },
    /// An agent's response ended and entered the conversation.
    UtteranceCommitted {
        /// The response.
        generation: u32,
        /// Its agent.
        agent: u8,
        /// All that the listener heard of it.
        text: String,
        /// Whether the agent was cut off.
        truncated: bool,
    },
    /// A response asked for a tool call.
    ToolCalled {
        /// The call.
        call: u64,
        /// What the tool is called.
        tool: String,
        /// The response that asked.
        generation: u32,
        /// Its arguments, where they may leave the server.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<String>,
    },
    /// A tool call was sent to its tool.
    ToolSent {
        /// The call.
        call: u64,
        /// 0 for the first time it is sent.
        attempt: u8,
    },
    /// A tool call ended.
    ToolFinished {
        /// The call.
        call: u64,
        /// How it ended.
        outcome: ToolOutcome,
        /// What the tool answered, where that may leave the server.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
    },
    /// An agent took the floor.
    AgentActivated {
        /// The agent.
        agent: u8,
    },
    /// A fact of a kind this version does not have.
    #[serde(other)]
    Unknown,
}

/// Something that is going on in a session, and is not kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum News {
    /// A participant started or stopped speaking.
    UserSpeaking {
        /// Who.
        who: u16,
        /// The turn the speech belongs to.
        turn: u32,
        /// Started, or stopped.
        speaking: bool,
    },
    /// What an agent is doing changed.
    Agent {
        /// The agent.
        agent: u8,
        /// The response concerned, when there is one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        generation: Option<u32>,
        /// What it is doing now.
        activity: Activity,
    },
    /// Text of a response has gone out. In a session of text it has gone
    /// to the reader. In a session that speaks it has gone to be spoken,
    /// ahead of what is heard: what was heard is in the facts.
    Said {
        /// The response.
        generation: u32,
        /// The text, which follows what was told of the response before.
        text: String,
    },
    /// What a participant has said of what they are saying, as far as it
    /// is recognised. It may still change: the words of a turn are the
    /// fact that commits it.
    Hearing {
        /// Who.
        who: u16,
        /// The words so far.
        text: String,
    },
    /// News of a kind this version does not have.
    #[serde(other)]
    Unknown,
}

/// One item of a conversation, as a client is shown it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item {
    /// What a participant said, or typed.
    Turn {
        /// The turn.
        turn: u32,
        /// Whose it is.
        who: u16,
        /// The words.
        text: String,
        /// Whether it was meant for the agent.
        addressed: bool,
    },
    /// Keys of a telephone that a participant pressed.
    Keys {
        /// The turn.
        turn: u32,
        /// Who pressed them.
        who: u16,
        /// The keys.
        keys: String,
    },
    /// What an agent said, as far as it was heard.
    Utterance {
        /// The response.
        generation: u32,
        /// The agent.
        agent: u8,
        /// The words the listener heard.
        text: String,
        /// Whether the agent was cut off.
        truncated: bool,
    },
    /// A tool call an agent asked for.
    ToolCall {
        /// The call.
        call: u64,
        /// What the tool is called.
        tool: String,
        /// The response that asked.
        generation: u32,
        /// Its arguments, where they may leave the server.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<String>,
    },
    /// How a tool call ended.
    ToolResult {
        /// The call.
        call: u64,
        /// How it ended.
        outcome: ToolOutcome,
        /// What the tool answered, where that may leave the server.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
    },
    /// An item of a kind this version does not have.
    #[serde(other)]
    Unknown,
}

/// A response that is being heard, as far as facts have said.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Speech {
    /// The response.
    pub generation: u32,
    /// Its agent.
    pub agent: u8,
    /// What has been heard of it.
    pub heard: String,
}

/// What the facts of a session up to one of them add up to, as a client
/// is shown it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// The number of the last fact the state includes.
    pub upto: u64,
    /// The stage of the session's life.
    pub lifecycle: Lifecycle,
    /// Why the session is closing, once it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_for: Option<CloseReason>,
    /// The agent that has the floor.
    pub agent: u8,
    /// The humans in the session.
    pub participants: Vec<u16>,
    /// The conversation, oldest item first.
    pub items: Vec<Item>,
    /// The response that is being heard, if one is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech: Option<Speech>,
}

/// What of a session's journal a client is shown beyond what was said
/// in it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Show {
    /// Whether the client is told of tool calls: it asked to be
    /// ([`features::TOOLS`]), and the server lets it.
    pub tools: bool,
    /// Whether the arguments of tool calls, and what the tools answered,
    /// may leave the server.
    pub tool_text: bool,
}

/// What the tool numbered `tool` is called, among the names of a
/// session's tools in the order its configuration has them. A tool that
/// has no name there is called by its number.
fn tool_name(tools: &[String], tool: u16) -> String {
    tools
        .get(usize::from(tool))
        .cloned()
        .unwrap_or_else(|| format!("#{tool}"))
}

impl From<journal::Lifecycle> for Lifecycle {
    fn from(stage: journal::Lifecycle) -> Self {
        match stage {
            journal::Lifecycle::Running => Self::Running,
            journal::Lifecycle::Draining => Self::Draining,
            journal::Lifecycle::Closing => Self::Closing,
            journal::Lifecycle::Closed => Self::Closed,
        }
    }
}

impl From<journal::CloseReason> for CloseReason {
    fn from(reason: journal::CloseReason) -> Self {
        match reason {
            journal::CloseReason::Requested => Self::Requested,
            journal::CloseReason::Abandoned => Self::Abandoned,
            journal::CloseReason::Failure => Self::Failure,
        }
    }
}

impl From<journal::EpochCause> for Cut {
    fn from(cause: journal::EpochCause) -> Self {
        match cause {
            journal::EpochCause::BargeIn => Self::BargeIn,
            journal::EpochCause::Interrupted => Self::Interrupted,
            journal::EpochCause::Handoff => Self::Handoff,
            journal::EpochCause::Superseded => Self::Superseded,
            journal::EpochCause::Failure => Self::Failure,
            journal::EpochCause::Recovery => Self::Recovery,
            journal::EpochCause::Closing => Self::Closing,
        }
    }
}

impl From<journal::Dismissal> for Dismissal {
    fn from(reason: journal::Dismissal) -> Self {
        match reason {
            journal::Dismissal::Backchannel => Self::Backchannel,
            journal::Dismissal::Noise => Self::Noise,
            journal::Dismissal::Empty => Self::Empty,
            journal::Dismissal::Closed => Self::Closed,
        }
    }
}

impl From<journal::ToolOutcome> for ToolOutcome {
    fn from(outcome: journal::ToolOutcome) -> Self {
        match outcome {
            journal::ToolOutcome::Completed => Self::Completed,
            journal::ToolOutcome::Failed => Self::Failed,
            journal::ToolOutcome::Cancelled => Self::Cancelled,
            journal::ToolOutcome::InDoubt => Self::InDoubt,
        }
    }
}

impl Fact {
    /// What a client is shown of a fact of a session's journal, if it is
    /// shown anything of it. `tools` are the names of the session's
    /// tools, in the order its configuration has them.
    ///
    /// Every kind of fact the journal has is decided here: a new one
    /// does not compile until it is said what a client sees of it.
    #[must_use]
    pub fn shown(fact: &journal::Fact<'_>, tools: &[String], show: Show) -> Option<Self> {
        let of_a_tool = matches!(
            fact,
            journal::Fact::ToolProposed { .. }
                | journal::Fact::ToolDispatched { .. }
                | journal::Fact::ToolFinished { .. }
        );
        if of_a_tool && !show.tools {
            return None;
        }
        Some(match fact {
            journal::Fact::ParticipantJoined { who } => Self::ParticipantJoined { who: *who },
            journal::Fact::ParticipantLeft { who } => Self::ParticipantLeft { who: *who },
            journal::Fact::LifecycleChanged { to, reason } => Self::LifecycleChanged {
                to: (*to).into(),
                reason: reason.map(Into::into),
            },
            // The number of an epoch is the server's. That something was
            // cut off, and why, is not.
            journal::Fact::EpochAdvanced { cause, .. } => Self::CutOff {
                cause: (*cause).into(),
            },
            journal::Fact::TurnCommitted {
                turn,
                who,
                text,
                addressed,
                ..
            } => Self::TurnCommitted {
                turn: *turn,
                who: *who,
                text: text.clone().into_owned(),
                addressed: *addressed,
            },
            journal::Fact::KeysCommitted { turn, who, keys } => Self::KeysCommitted {
                turn: *turn,
                who: *who,
                keys: keys.clone().into_owned(),
            },
            journal::Fact::TurnDismissed { turn, who, reason } => Self::TurnDismissed {
                turn: *turn,
                who: *who,
                reason: (*reason).into(),
            },
            journal::Fact::UtteranceProgress {
                generation,
                agent,
                heard,
            } => Self::UtteranceProgress {
                generation: *generation,
                agent: *agent,
                heard: heard.clone().into_owned(),
            },
            journal::Fact::UtteranceCommitted {
                generation,
                agent,
                text,
                truncated,
                ..
            } => Self::UtteranceCommitted {
                generation: *generation,
                agent: *agent,
                text: text.clone().into_owned(),
                truncated: *truncated,
            },
            journal::Fact::ToolProposed {
                call,
                generation,
                tool,
                args,
                ..
            } => Self::ToolCalled {
                call: *call,
                tool: tool_name(tools, *tool),
                generation: *generation,
                args: show.tool_text.then(|| args.clone().into_owned()),
            },
            journal::Fact::ToolDispatched { call, attempt } => Self::ToolSent {
                call: *call,
                attempt: *attempt,
            },
            journal::Fact::ToolFinished {
                call,
                outcome,
                result,
            } => Self::ToolFinished {
                call: *call,
                outcome: (*outcome).into(),
                result: show.tool_text.then(|| result.clone().into_owned()),
            },
            journal::Fact::AgentActivated { agent } => Self::AgentActivated { agent: *agent },
            // What is said to the language model and not to anyone else,
            // and the server's own bookkeeping of tool calls and of who
            // is to take the floor.
            journal::Fact::NoticeAppended { .. }
            | journal::Fact::ToolAuthorized { .. }
            | journal::Fact::BatchResolved { .. }
            | journal::Fact::HandoffRequested { .. } => return None,
        })
    }
}

impl Item {
    /// What a client is shown of an item of a stored conversation, if it
    /// is shown anything of it.
    fn shown(item: &journal::Item<'_>, tools: &[String], show: Show) -> Option<Self> {
        let of_a_tool = matches!(
            item,
            journal::Item::ToolCall { .. } | journal::Item::ToolResult { .. }
        );
        if of_a_tool && !show.tools {
            return None;
        }
        Some(match item {
            journal::Item::UserTurn {
                turn,
                who,
                text,
                addressed,
            } => Self::Turn {
                turn: *turn,
                who: *who,
                text: text.clone().into_owned(),
                addressed: *addressed,
            },
            journal::Item::Keys { turn, who, keys } => Self::Keys {
                turn: *turn,
                who: *who,
                keys: keys.clone().into_owned(),
            },
            journal::Item::Utterance {
                generation,
                agent,
                text,
                truncated,
            } => Self::Utterance {
                generation: *generation,
                agent: *agent,
                text: text.clone().into_owned(),
                truncated: *truncated,
            },
            journal::Item::ToolCall {
                call,
                tool,
                generation,
                args,
            } => Self::ToolCall {
                call: *call,
                tool: tool_name(tools, *tool),
                generation: *generation,
                args: show.tool_text.then(|| args.clone().into_owned()),
            },
            journal::Item::ToolResult {
                call,
                outcome,
                text,
            } => Self::ToolResult {
                call: *call,
                outcome: (*outcome).into(),
                result: show.tool_text.then(|| text.clone().into_owned()),
            },
            journal::Item::Notice { .. } => return None,
        })
    }
}

impl State {
    /// What a client is shown of a session's stored state: what the
    /// facts it was shown up to there add up to.
    #[must_use]
    pub fn shown(state: &journal::State<'_>, tools: &[String], show: Show) -> Self {
        Self {
            upto: state.upto,
            lifecycle: state.lifecycle.into(),
            closed_for: state.closed_for.map(Into::into),
            agent: state.front,
            participants: state.participants.clone(),
            items: state
                .items
                .iter()
                .filter_map(|item| Item::shown(item, tools, show))
                .collect(),
            speech: state.speech.as_ref().map(|speech| Speech {
                generation: speech.generation,
                agent: speech.agent,
                heard: speech.heard.clone().into_owned(),
            }),
        }
    }
}

/// What a client holds of a session: what the facts it was sent add up
/// to. It is what a [`State`] says, and goes on from one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    state: State,
    /// Whether facts were missed that no state has made up for yet.
    behind: bool,
}

/// What a message did to a [`View`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Applied {
    /// It is a fact the view did not have, and has now.
    Fact,
    /// It is a fact the view had already. Nothing changed.
    Seen,
    /// It is a state, which the view is now.
    State,
    /// It says that facts were missed. The view is behind until a state
    /// arrives.
    Gap,
    /// It is not about what a view holds.
    Nothing,
}

impl View {
    /// The view of a session of which nothing is known yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The view as a state says it.
    #[must_use]
    pub const fn from_state(state: State) -> Self {
        Self {
            state,
            behind: false,
        }
    }

    /// What the view holds, as a state says it.
    #[must_use]
    pub const fn state(&self) -> &State {
        &self.state
    }

    /// The number of the last fact the view includes: what a client that
    /// comes back says it has seen.
    #[must_use]
    pub const fn seen(&self) -> u64 {
        self.state.upto
    }

    /// Whether facts were missed that no state has made up for yet. What
    /// the view holds is then older than the session.
    #[must_use]
    pub const fn is_behind(&self) -> bool {
        self.behind
    }

    /// Takes what a server said.
    pub fn apply(&mut self, message: &FromServer) -> Applied {
        match message {
            FromServer::Fact { seq, fact } => {
                if *seq <= self.state.upto {
                    return Applied::Seen;
                }
                self.state.upto = *seq;
                self.fact(fact);
                Applied::Fact
            }
            FromServer::State(state) => {
                self.state = state.clone();
                self.behind = false;
                Applied::State
            }
            FromServer::Gap { .. } => {
                self.behind = true;
                Applied::Gap
            }
            _ => Applied::Nothing,
        }
    }

    /// Makes a fact true of the view.
    fn fact(&mut self, fact: &Fact) {
        let state = &mut self.state;
        match fact {
            Fact::ParticipantJoined { who } => {
                if !state.participants.contains(who) {
                    state.participants.push(*who);
                }
            }
            Fact::ParticipantLeft { who } => state.participants.retain(|known| known != who),
            Fact::LifecycleChanged { to, reason } => {
                state.lifecycle = *to;
                state.closed_for = reason.or(state.closed_for);
            }
            Fact::TurnCommitted {
                turn,
                who,
                text,
                addressed,
            } => state.items.push(Item::Turn {
                turn: *turn,
                who: *who,
                text: text.clone(),
                addressed: *addressed,
            }),
            Fact::KeysCommitted { turn, who, keys } => state.items.push(Item::Keys {
                turn: *turn,
                who: *who,
                keys: keys.clone(),
            }),
            Fact::UtteranceProgress {
                generation,
                agent,
                heard,
            } => match &mut state.speech {
                Some(speech) if speech.generation == *generation && speech.agent == *agent => {
                    speech.heard.push_str(heard);
                }
                speech => {
                    *speech = Some(Speech {
                        generation: *generation,
                        agent: *agent,
                        heard: heard.clone(),
                    });
                }
            },
            Fact::UtteranceCommitted {
                generation,
                agent,
                text,
                truncated,
            } => {
                state.items.push(Item::Utterance {
                    generation: *generation,
                    agent: *agent,
                    text: text.clone(),
                    truncated: *truncated,
                });
                if state
                    .speech
                    .as_ref()
                    .is_some_and(|speech| speech.generation == *generation)
                {
                    state.speech = None;
                }
            }
            Fact::ToolCalled {
                call,
                tool,
                generation,
                args,
            } => state.items.push(Item::ToolCall {
                call: *call,
                tool: tool.clone(),
                generation: *generation,
                args: args.clone(),
            }),
            Fact::ToolFinished {
                call,
                outcome,
                result,
            } => state.items.push(Item::ToolResult {
                call: *call,
                outcome: *outcome,
                result: result.clone(),
            }),
            Fact::AgentActivated { agent } => state.agent = *agent,
            // What happened and left nothing in what a view holds.
            Fact::CutOff { .. }
            | Fact::TurnDismissed { .. }
            | Fact::ToolSent { .. }
            | Fact::Unknown => {}
        }
    }
}

/// Why a message of a client says something a message of its kind
/// cannot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Refusal {
    /// The hello names a version this server does not speak.
    Version,
    /// The audio has a rate that is not one of [`RATES`].
    Rate,
    /// The token does not begin as a token does.
    Token,
    /// There is no text, or more than [`MOST_TEXT`] bytes of it.
    Text,
    /// There are no keys, more than [`MOST_KEYS`], or something that is
    /// not a key.
    Keys,
    /// The message is of a kind this version does not have.
    Unknown,
}

impl Refusal {
    /// The code a client is told.
    #[must_use]
    pub const fn code(self) -> ErrorCode {
        match self {
            Self::Version => ErrorCode::Version,
            Self::Rate => ErrorCode::Unsupported,
            Self::Token => ErrorCode::Token,
            Self::Text | Self::Keys => ErrorCode::Malformed,
            Self::Unknown => ErrorCode::UnknownMessage,
        }
    }
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Version => "the hello names a version of the protocol that is not spoken here",
            Self::Rate => "the audio has a rate that is not offered",
            Self::Token => "a token does not begin as one does",
            Self::Text => "the text is empty or too long",
            Self::Keys => "the keys are none, too many, or not keys of a telephone",
            Self::Unknown => "the message is of a kind that is not known here",
        })
    }
}

impl core::error::Error for Refusal {}

/// Whether `text` is something that can be typed: not nothing, and not
/// more than [`MOST_TEXT`] bytes.
pub(crate) fn checked_text(text: &str) -> Result<(), Refusal> {
    if text.trim().is_empty() || text.len() > MOST_TEXT {
        return Err(Refusal::Text);
    }
    Ok(())
}

/// Whether `keys` are keys of a telephone: one at least, and not more
/// than [`MOST_KEYS`].
pub(crate) fn checked_keys(keys: &str) -> Result<(), Refusal> {
    // A key is one byte, so the bytes are counted and no key is looked
    // at twice.
    let all = keys.bytes().all(|key| KEYS.as_bytes().contains(&key));
    if keys.is_empty() || keys.len() > MOST_KEYS || !all {
        return Err(Refusal::Keys);
    }
    Ok(())
}

impl FromClient {
    /// Whether the message says something a message of its kind can.
    /// Whether it comes at a place where it can, and whether a token is
    /// good, is for whoever holds the session to say.
    ///
    /// # Errors
    ///
    /// What is wrong with it.
    pub fn check(&self) -> Result<(), Refusal> {
        match self {
            Self::Hello {
                v,
                token,
                audio,
                resume,
                ..
            } => {
                if *v != VERSION {
                    return Err(Refusal::Version);
                }
                if audio.is_some_and(|audio| !RATES.contains(&audio.rate)) {
                    return Err(Refusal::Rate);
                }
                let client = token.as_ref().is_none_or(|token| {
                    token.len() > CLIENT_TOKEN.len() && token.starts_with(CLIENT_TOKEN)
                });
                let resume = resume.as_ref().is_none_or(|resume| {
                    resume.token.len() > RESUME_TOKEN.len()
                        && resume.token.starts_with(RESUME_TOKEN)
                });
                if client && resume {
                    Ok(())
                } else {
                    Err(Refusal::Token)
                }
            }
            Self::Text { text } => checked_text(text),
            Self::Dtmf { keys } => checked_keys(keys),
            Self::Played { .. } | Self::PlayedRtp { .. } | Self::Interrupt | Self::Bye => Ok(()),
            Self::Unknown => Err(Refusal::Unknown),
        }
    }
}
