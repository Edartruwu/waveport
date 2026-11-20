//! RTVI, version 2.3.0: the messages of Pipecat's client SDKs.
//!
//! RTVI is what an application written for Pipecat's clients says to
//! its server and is told by it. It is not this server's protocol:
//! that is [client protocol v1](crate::client::v1), and a gateway puts
//! one into the other. Here are the messages the gateway reads and the
//! ones it writes, and nothing of what they mean.
//!
//! This was read on 20 November 2026 from the source of Pipecat 1.16.0
//! (`processors/frameworks/rtvi`) and of `@pipecat-ai/client-js`
//! 1.16.0, which are the standard as it is practised.
//!
//! # A message
//!
//! One JSON object: `label`, which is always [`LABEL`]; `type`, which
//! says what it is; `id`, which every message of a client has and which
//! an answer repeats; and `data`, whose shape `type` decides and which
//! is left out where there is none.
//!
//! # Reading
//!
//! [`read`] takes what a client wrote. The `type` decides how `data`
//! is read and may come after it, and a kind that is not known has data
//! of any shape, so a message is read more than once: for its label,
//! for its kind and id, and for the data of its kind. This crate reads
//! no JSON itself; whoever calls says how ([`Json`]).
//!
//! Three things are told apart, because each is answered differently.
//! What is not labelled as RTVI is not RTVI ([`Written::Foreign`]): a
//! channel carries other things. What is labelled and has no kind or no
//! id cannot be answered by its id ([`Written::Unreadable`]). Anything
//! else is a message, and where its data is not what its kind takes, or
//! its kind is not known, it says so ([`Asks::Invalid`],
//! [`Asks::Other`]), so that it can be refused by its id.
//!
//! A message is an object, and so is whatever of its data has fields.
//! The same fields written one after the other in an array, which a
//! reader of JSON would take for them, are not.
//!
//! # Writing
//!
//! [`ToClient`] is what a client is told, and writes as the message it
//! is. A field that has nothing is left out, as Pipecat leaves it out.
//! JSON that came from elsewhere, the arguments of a tool call and what
//! a tool answered, is of whatever type the writer holds JSON in.
//!
//! # What is not here
//!
//! The messages the gateway never sends and the data of the ones it
//! only refuses (decision 0006 names them). The second format of
//! `bot-output`, which is that of protocol 1.

use core::fmt;
use core::marker::PhantomData;

use serde::de::value::MapAccessDeserializer;
use serde::de::{DeserializeOwned, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

/// What every message is labelled with.
pub const LABEL: &str = "rtvi-ai";

/// The version of the protocol that is written here.
pub const VERSION: &str = "2.3.0";

/// What reads JSON for [`read`].
pub trait Json {
    /// `text` as a `T`, or nothing where it is none.
    fn read<T: DeserializeOwned>(text: &str) -> Option<T>;
}

/// What a client wrote on a channel RTVI is spoken on, as far as RTVI
/// goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Written {
    /// Not something labelled as RTVI.
    Foreign,
    /// Labelled as RTVI, and no message: its kind or its id is not
    /// there.
    Unreadable,
    /// A message.
    Said(FromClient),
}

/// A message of a client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FromClient {
    /// Its id, which what answers it repeats.
    pub id: String,
    /// What it asks.
    pub asks: Asks,
}

/// What a message of a client asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Asks {
    /// `client-ready`: the client can be spoken to. It says what it
    /// can of itself: a client that says nothing is ready all the same.
    ClientReady {
        /// The version of the protocol it speaks.
        version: Option<String>,
        /// What it is.
        about: Option<About>,
    },
    /// `disconnect-bot`: the session is to end.
    DisconnectBot,
    /// `send-text`: something typed.
    SendText(SendText),
    /// `dtmf`: keys of a telephone, in order, each as the text of one
    /// key.
    Dtmf(Vec<String>),
    /// A message of one of the kinds above whose data is not what that
    /// kind takes. This is the `type` it has.
    Invalid(&'static str),
    /// A message of another kind, with its `type`.
    Other(String),
}

/// What a client says of itself.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct About {
    /// The library it is built on.
    pub library: String,
    /// The version of that library.
    #[serde(default)]
    pub library_version: Option<String>,
    /// The system it runs on.
    #[serde(default)]
    pub platform: Option<String>,
}

/// Something typed.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct SendText {
    /// The text.
    pub content: String,
    /// How it is to be taken.
    #[serde(default, deserialize_with = "object")]
    pub options: SendTextOptions,
}

/// How typed text is to be taken. What is not said is true.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub struct SendTextOptions {
    /// Whether it is answered now.
    #[serde(default = "yes")]
    pub run_immediately: bool,
    /// Whether the answer is spoken.
    #[serde(default = "yes")]
    pub audio_response: bool,
}

impl Default for SendTextOptions {
    fn default() -> Self {
        Self {
            run_immediately: true,
            audio_response: true,
        }
    }
}

const fn yes() -> bool {
    true
}

/// A `T` that is written as an object.
struct Object<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Fields<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for Fields<T> {
            type Value = T;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object")
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map))
            }
        }

        deserializer.deserialize_map(Fields(PhantomData)).map(Self)
    }
}

/// Reads a field that is an object.
fn object<'de, D: Deserializer<'de>, T: Deserialize<'de>>(deserializer: D) -> Result<T, D::Error> {
    Object::deserialize(deserializer).map(|Object(read)| read)
}

/// Whatever is labelled.
#[derive(Deserialize)]
struct Labelled {
    label: String,
}

/// What every message of a client has.
#[derive(Deserialize)]
struct Header {
    #[serde(rename = "type")]
    kind: String,
    id: String,
}

/// A message, for its data alone.
#[derive(Deserialize)]
struct Carries<T> {
    data: T,
}

#[derive(Deserialize)]
struct Versioned {
    version: String,
}

#[derive(Deserialize)]
struct Introduced {
    #[serde(deserialize_with = "object")]
    about: About,
}

#[derive(Deserialize)]
struct Pressed {
    buttons: Vec<String>,
}

/// `text` as a `T` that is written as an object.
fn whole<J: Json, T: DeserializeOwned>(text: &str) -> Option<T> {
    J::read::<Object<T>>(text).map(|Object(read)| read)
}

/// The data of a message, read as that of its kind.
fn data<J: Json, T: DeserializeOwned>(text: &str) -> Option<T> {
    whole::<J, Carries<Object<T>>>(text).map(|carries| carries.data.0)
}

/// Reads what a client wrote.
#[must_use]
pub fn read<J: Json>(text: &str) -> Written {
    if whole::<J, Labelled>(text).is_none_or(|labelled| labelled.label != LABEL) {
        return Written::Foreign;
    }
    let Some(Header { kind, id }) = whole::<J, Header>(text) else {
        return Written::Unreadable;
    };
    let asks = match kind.as_str() {
        "client-ready" => Asks::ClientReady {
            version: data::<J, Versioned>(text).map(|data| data.version),
            about: data::<J, Introduced>(text).map(|data| data.about),
        },
        "disconnect-bot" => Asks::DisconnectBot,
        "send-text" => data::<J, SendText>(text).map_or(Asks::Invalid("send-text"), Asks::SendText),
        "dtmf" => data::<J, Pressed>(text)
            .map_or(Asks::Invalid("dtmf"), |pressed| Asks::Dtmf(pressed.buttons)),
        _ => Asks::Other(kind),
    };
    Written::Said(FromClient { id, asks })
}

/// A message to a client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ToClient<V = ()> {
    label: &'static str,
    /// What it says.
    #[serde(flatten)]
    pub says: Says<V>,
    /// The id of the message of the client that it answers, where it
    /// answers one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

impl<V> ToClient<V> {
    /// Something the client is told, which answers nothing it said.
    #[must_use]
    pub const fn told(says: Says<V>) -> Self {
        Self {
            label: LABEL,
            says,
            id: None,
        }
    }

    /// The answer to the message of the client that has `id`.
    #[must_use]
    pub const fn answer(id: String, says: Says<V>) -> Self {
        Self {
            label: LABEL,
            says,
            id: Some(id),
        }
    }
}

/// What a client is told. `V` is what the writer holds JSON in that
/// came from elsewhere.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "kebab-case")]
pub enum Says<V = ()> {
    /// The answer to `client-ready`.
    BotReady(BotReady),
    /// Something failed that no message of the client asked for.
    Error {
        /// What, for a person.
        error: String,
        /// Whether the session is ending for it.
        fatal: bool,
    },
    /// A message of the client was not taken, or not in full.
    ErrorResponse {
        /// Why, for a person.
        error: String,
    },
    /// A participant's speech began.
    UserStartedSpeaking,
    /// A participant's speech stopped.
    UserStoppedSpeaking,
    /// The far end starts to hear a response.
    BotStartedSpeaking,
    /// The far end has heard the last of a response, or it was cut.
    BotStoppedSpeaking,
    /// A response was cut off by the user.
    BotInterrupted,
    /// What a participant said or typed.
    UserInput(UserInput),
    /// What a participant said. Deprecated in favour of `user-input`.
    UserTranscription(UserTranscription),
    /// Text of a response, and how much of it has been heard.
    BotOutput(BotOutput),
    /// A response is asked of the model.
    BotLlmStarted,
    /// The model has finished.
    BotLlmStopped,
    /// Text of a response as the model wrote it. Deprecated in favour
    /// of `bot-output`.
    BotLlmText {
        /// The text.
        text: String,
    },
    /// The first text of a response goes to be spoken.
    BotTtsStarted,
    /// The last audio of a response has arrived.
    BotTtsStopped,
    /// Text of a response as it was spoken. Deprecated in favour of
    /// `bot-output`.
    BotTtsText {
        /// The text.
        text: String,
    },
    /// A response asks for a tool call.
    LlmFunctionCallStarted {
        /// What the tool is called.
        #[serde(skip_serializing_if = "Option::is_none")]
        function_name: Option<String>,
    },
    /// A tool call was sent to its tool.
    LlmFunctionCallInProgress {
        /// The call.
        tool_call_id: String,
        /// What the tool is called.
        #[serde(skip_serializing_if = "Option::is_none")]
        function_name: Option<String>,
        /// Its arguments, which are a JSON object, where they may leave
        /// the server.
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments: Option<V>,
    },
    /// A tool call ended.
    LlmFunctionCallStopped {
        /// The call.
        tool_call_id: String,
        /// Whether it ended without an answer.
        cancelled: bool,
        /// What the tool is called.
        #[serde(skip_serializing_if = "Option::is_none")]
        function_name: Option<String>,
        /// What the tool answered, where that may leave the server.
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<V>,
    },
}

/// The answer to `client-ready`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BotReady {
    /// The version of the protocol the client is spoken to in.
    pub version: String,
    /// What answers.
    pub about: AboutBot,
    /// What a session does.
    pub capabilities: Capabilities,
}

/// What answers a client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AboutBot {
    /// The library.
    pub library: String,
    /// Its version.
    pub library_version: String,
}

/// What a session does, so that a client shows only what applies.
#[expect(
    clippy::struct_excessive_bools,
    reason = "the six are the standard's, each a thing a session does or does not"
)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    /// It takes the client's audio.
    pub audio_in: bool,
    /// It sends audio.
    pub audio_out: bool,
    /// It takes the client's camera.
    pub video_in: bool,
    /// It takes the client's screen.
    pub screen_in: bool,
    /// It sends video.
    pub video_out: bool,
    /// It sends metrics.
    pub metrics: bool,
}

/// What kind of input a `user-input` is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InputType {
    /// Speech, as it was recognised.
    Transcription,
    /// Text the client sent.
    Chat,
    /// A noise of agreement while the agent speaks.
    Backchannel,
}

/// What a participant said or typed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UserInput {
    /// The words.
    pub text: String,
    /// What kind of input it is.
    pub input_type: InputType,
    /// When, as a time of ISO 8601.
    pub timestamp: String,
    /// Whether the words are final. Only speech that is still being
    /// recognised is not.
    #[serde(rename = "final")]
    pub is_final: bool,
    /// Who.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// For text the client sent, the id of its `send-text`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<String>,
}

/// What a participant said, as the deprecated message has it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UserTranscription {
    /// The words.
    pub text: String,
    /// Who.
    pub user_id: String,
    /// When, as a time of ISO 8601.
    pub timestamp: String,
    /// Whether the words are final.
    #[serde(rename = "final")]
    pub is_final: bool,
}

/// Where a stretch of a response is on its way to being heard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpokenStatus {
    /// It has gone out: the first message of its stretch, and the only
    /// one of text that is not spoken.
    New,
    /// More of it was heard.
    InProgress,
    /// All of it was heard.
    Completed,
}

/// How much of a stretch was heard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SpokenProgress {
    /// What was heard of it so far.
    pub accumulated_text: String,
    /// What was not yet.
    pub remaining_text: String,
}

/// The form the text of a `bot-output` is in where it is a stretch that
/// was released to be spoken as one.
pub const SENTENCE: &str = "sentence";

/// Text of a response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BotOutput {
    /// The stretch where it is new, and what was heard since the
    /// message before where it is being heard.
    pub text: String,
    /// The form the text is in.
    pub output_type: &'static str,
    /// The same, under the first of its two deprecated names, which
    /// Pipecat still fills.
    pub text_type: &'static str,
    /// The same, under the second.
    pub aggregated_by: &'static str,
    /// The stretch, which its messages share.
    pub segment_id: u64,
    /// Whether it will be spoken.
    pub will_be_spoken: bool,
    /// Where it is.
    pub spoken_status: SpokenStatus,
    /// How much of it was heard. Text that is not spoken has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spoken_progress: Option<SpokenProgress>,
}

impl BotOutput {
    /// A message about the stretch `segment_id`, which is a
    /// [`SENTENCE`].
    #[must_use]
    pub const fn of(
        segment_id: u64,
        text: String,
        will_be_spoken: bool,
        spoken_status: SpokenStatus,
        spoken_progress: Option<SpokenProgress>,
    ) -> Self {
        Self {
            text,
            output_type: SENTENCE,
            text_type: SENTENCE,
            aggregated_by: SENTENCE,
            segment_id,
            will_be_spoken,
            spoken_status,
            spoken_progress,
        }
    }
}
