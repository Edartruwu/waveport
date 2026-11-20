//! The RTVI gateway: what a client of Pipecat's says and is told, put
//! into client protocol v1 and out of it.
//!
//! An application written for Pipecat's clients speaks RTVI on the data
//! channel of its call. A session speaks client protocol v1. A
//! [`Gateway`] stands between the two for one call (decision 0006 says
//! what it does with each message). It is a function of what it was
//! given and of the time of day: it does nothing but keep count.
//!
//! # What v1 says and RTVI wants to hear
//!
//! | RTVI | Said when v1 says |
//! |---|---|
//! | `user-started-speaking`, `user-stopped-speaking` | the news that a participant's speech began or stopped |
//! | `user-input`, not final, and `user-transcription` | the news of what is recognised so far |
//! | `user-input`, final, and for speech `user-transcription` | the fact that a turn was committed. It is `chat`, with the id of its `send-text`, where the words are ones the client sent |
//! | `user-input`, `backchannel` | the fact that a turn was dismissed as a noise of agreement, where words of it were recognised |
//! | `bot-llm-started` | the news that the agent is working on a response |
//! | `bot-llm-text`, `bot-output` `new` | the news of text that has gone to be spoken: each piece is a stretch of its own. It is the model's text where the agent was working on a response, and not where it says what it was given to say, as a greeting is |
//! | `bot-tts-started` | the first text of a response that has gone to be spoken, or the mark that begins its audio where no text was told before it |
//! | `bot-started-speaking` | the news that the agent is speaking: the far end starts to hear it |
//! | `bot-tts-text`, `bot-output` `in-progress` and `completed` | the fact that more of a response was heard, and the fact that it ended for what was heard since, laid over the stretches that went out |
//! | `bot-llm-stopped`, `bot-tts-stopped` | the mark that ends the audio of a response |
//! | `bot-interrupted` | the fact that a response was cut off by somebody speaking or by the application |
//! | `bot-stopped-speaking` | the news that the agent listens again |
//! | `llm-function-call-started`, `-in-progress`, `-stopped` | the facts that a tool call was asked for, sent and ended |
//! | `error` | an error, which is fatal where the far end was never welcomed, and the fact that the session is closing because it failed |
//!
//! Two of these are nearer than they are exact. A session does not
//! tell its clients when the model has finished, so `bot-llm-stopped`
//! is said when the last audio of the response has been made, which is
//! later. And what was heard of a response is told for the response,
//! not for a stretch of it: it is laid over the stretches by counting
//! characters, and a response that was heard to its end has every
//! stretch completed whatever the count says.
//!
//! When the session says goodbye the client is told that its peer has
//! left, in the words of its transport, which are not RTVI
//! ([`PEER_LEFT`]). A client that is told so ends its call. One that is
//! not takes the end of the call for a network that failed, and calls
//! again: so a call that ends because the client made another in its
//! place is ended without a word, and a client that says goodbye
//! itself (`disconnect-bot`) is told the same as its answer.
//!
//! # What RTVI says and v1 wants to hear
//!
//! `send-text` is typed text, `dtmf` keys, `disconnect-bot` a goodbye.
//! `client-ready` is answered here and means nothing to a session.
//! Everything else is refused by its id. What is not labelled as RTVI
//! is the transport's own (a `ping` once a second, the state of the
//! client's tracks) and is passed over.

use core::mem::{replace, take};
use std::collections::{BTreeMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde_json::Value;
use waveport_protocol::client::v1::{
    Activity, ByeReason, CloseReason, Cut, Dismissal, Fact, FromClient, FromServer, Lifecycle,
    News, ToolOutcome,
};
use waveport_protocol::rtvi::{
    self, AboutBot, Asks, BotOutput, BotReady, Capabilities, InputType, Says, SpokenProgress,
    SpokenStatus, ToClient, UserInput, UserTranscription, Written,
};
use waveport_transport_webrtc::{Dialect, Meant};

/// The data channel a client of Pipecat's opens.
pub(crate) const CHANNEL: &str = "chat";

/// What the client's transport is told when the session is over: its
/// peer has left. It is the transport's message and not RTVI's.
pub(crate) const PEER_LEFT: &str = r#"{"type":"signalling","message":{"type":"peerLeft"}}"#;

/// How many responses are kept count of at once. One is heard at a
/// time; one that was given up for a newer is never said to be over.
const RESPONSES: usize = 4;

/// How many texts the client sent are remembered until the turns they
/// become are committed.
const TYPED: usize = 16;

/// How many tool calls that have not ended are remembered by name.
const TOOLS: usize = 32;

/// How many participants' words are remembered while they speak.
const SPEAKERS: usize = 8;

/// JSON as it is read here.
struct Json;

impl rtvi::Json for Json {
    fn read<T: DeserializeOwned>(text: &str) -> Option<T> {
        serde_json::from_str(text).ok()
    }
}

/// What is to be written to the client, in order.
#[derive(Default)]
struct Out(Vec<String>);

impl Out {
    fn write(&mut self, message: &ToClient<Value>) {
        // A message of these types is always written.
        self.0.extend(serde_json::to_string(message).ok());
    }

    /// Tells the client something.
    fn say(&mut self, says: Says<Value>) {
        self.write(&ToClient::told(says));
    }

    /// Answers the message of the client that has `id`.
    fn answer(&mut self, id: &str, says: Says<Value>) {
        self.write(&ToClient::answer(id.to_owned(), says));
    }

    /// Refuses the message of the client that has `id`.
    fn refuse(&mut self, id: &str, why: impl Into<String>) {
        let error = why.into();
        self.answer(id, Says::ErrorResponse { error });
    }
}

/// `text` as its first `chars` characters and the rest.
fn split(text: &str, chars: usize) -> (&str, &str) {
    let at = text
        .char_indices()
        .nth(chars)
        .map_or(text.len(), |(at, _)| at);
    text.split_at_checked(at).unwrap_or((text, ""))
}

/// A stretch of a response: text that went out to be spoken as one.
struct Segment {
    id: u64,
    text: String,
    /// How long it is, in characters.
    chars: usize,
    /// How many of them the client was told were heard.
    told: usize,
}

/// A response, as far as it went out and was heard.
#[derive(Default)]
struct Response {
    segments: Vec<Segment>,
    /// How many characters of it were heard.
    heard: usize,
}

impl Response {
    /// Tells the client of what was heard of each stretch since it was
    /// last told: all of each, if the response was heard to its end.
    fn tell(&mut self, out: &mut Out, to_its_end: bool) {
        let mut before = 0_usize;
        for segment in &mut self.segments {
            let heard = if to_its_end {
                segment.chars
            } else {
                self.heard.saturating_sub(before).min(segment.chars)
            };
            before = before.saturating_add(segment.chars);
            if heard <= segment.told {
                continue;
            }
            let (accumulated, remaining) = split(&segment.text, heard);
            let (_, new) = split(accumulated, segment.told);
            let status = if heard == segment.chars {
                SpokenStatus::Completed
            } else {
                SpokenStatus::InProgress
            };
            let progress = SpokenProgress {
                accumulated_text: accumulated.to_owned(),
                remaining_text: remaining.to_owned(),
            };
            let output = BotOutput::of(segment.id, new.to_owned(), true, status, Some(progress));
            out.say(Says::BotOutput(output));
            segment.told = heard;
        }
    }
}

/// A tool call that has not ended.
struct Tool {
    name: String,
    /// Its arguments, where they may leave the server.
    args: Option<String>,
}

/// What the client was told had begun, and not yet that it ended.
#[derive(Default)]
struct Begun {
    /// The model was asked.
    thinking: bool,
    /// The audio of a response is being made.
    synthesising: bool,
    /// The far end hears a response.
    speaking: bool,
}

/// What stands between one call of a client of Pipecat's and its
/// session.
pub(crate) struct Gateway {
    /// The time of day.
    now: fn() -> SystemTime,
    /// Whether the far end was welcomed to a session.
    welcomed: bool,
    /// Whether the client was told that its session failed.
    failed: bool,
    /// How many stretches went out.
    segments: u64,
    /// The responses that are under way, by their generation.
    responses: BTreeMap<u32, Response>,
    /// What the client sent as text and no turn was committed of yet:
    /// the id of the message, and the text.
    typed: VecDeque<(String, String)>,
    /// What is recognised of what each participant is saying.
    hearing: BTreeMap<u16, String>,
    /// What of a response the client was told had begun.
    begun: Begun,
    /// The tool calls that have not ended.
    tools: BTreeMap<u64, Tool>,
}

/// Why a client that says it speaks `version` is not served in full,
/// if it is not.
fn not_in_full(version: Option<&str>) -> Option<&'static str> {
    let Some(version) = version else {
        return Some(
            "the client does not say which version of RTVI it speaks: it is spoken to in \
             version 2.3.0, and may not understand all of it",
        );
    };
    let mut parts = version.split('.').map(str::parse::<u32>);
    let parts = [parts.next(), parts.next(), parts.next(), parts.next()];
    match parts {
        [Some(Ok(2)), Some(Ok(_)), Some(Ok(_)), None] => None,
        [Some(Ok(1)), Some(Ok(_)), Some(Ok(_)), None] => Some(
            "version 1 of RTVI is spoken to in the format of version 2.3.0 here: `bot-output` \
             is not what a client of version 1 expects",
        ),
        [Some(Ok(_)), Some(Ok(_)), Some(Ok(_)), None] => Some(
            "the client speaks a version of RTVI that is not compatible with 2.3.0, which is \
             spoken here: it may not understand all of it",
        ),
        _ => Some(
            "the client names a version of RTVI that is none: it is spoken to in version \
             2.3.0, and may not understand all of it",
        ),
    }
}

/// Why a message of the kind `kind`, which is not acted on, is not.
fn not_served(kind: &str) -> &'static str {
    match kind {
        "send-file" => "`send-file` is not served: a session takes no files",
        "llm-function-call-result" => {
            "`llm-function-call-result` is not served: tools are run by the server, and none \
             by a client"
        }
        "client-message" => {
            "`client-message` is not served: there is nothing here for an application to be \
             answered by"
        }
        "ui-snapshot" | "ui-event" | "ui-cancel-job-group" => "the UI Agent Protocol is not served",
        "append-to-context" => {
            "`append-to-context` is deprecated and not served: text is sent with `send-text`"
        }
        "raw-audio" | "raw-audio-batch" => {
            "audio is not taken as messages: it is the audio of the call"
        }
        _ => "a message of this type is not known here",
    }
}

/// What `buttons` name, if each is one character: whether each is a
/// key of a telephone is the protocol's to say.
fn keys(buttons: &[String]) -> Option<String> {
    buttons
        .iter()
        .map(|button| {
            let mut written = button.chars();
            let key = written.next()?;
            written.next().is_none().then_some(key)
        })
        .collect()
}

/// When `at` is, as ISO 8601 says it, to the millisecond.
fn stamp(at: SystemTime) -> String {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let (seconds, millis) = (since.as_secs(), since.subsec_millis());
    let (days, of_day) = (seconds / 86_400, seconds % 86_400);
    // The day of the calendar that is so many days after 1 January
    // 1970, counted in eras of 400 years from 1 March of year 0.
    let shifted = days + 719_468;
    let (era, of_era) = (shifted / 146_097, shifted % 146_097);
    let year_of_era = (of_era - of_era / 1_460 + of_era / 36_524 - of_era / 146_096) / 365;
    let of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let from_march = (5 * of_year + 2) / 153;
    let day = of_year - (153 * from_march + 2) / 5 + 1;
    let month = if from_march < 10 {
        from_march + 3
    } else {
        from_march - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    let (hour, minute, second) = (of_day / 3_600, of_day % 3_600 / 60, of_day % 60);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// JSON that came from a tool, as JSON where it is that and as text
/// where it is not.
fn answered(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
}

impl Gateway {
    /// A gateway for one call.
    pub(crate) fn new() -> Self {
        Self::at(SystemTime::now)
    }

    /// The same, which takes the time of day from `now`.
    fn at(now: fn() -> SystemTime) -> Self {
        Self {
            now,
            welcomed: false,
            failed: false,
            segments: 0,
            responses: BTreeMap::new(),
            typed: VecDeque::new(),
            hearing: BTreeMap::new(),
            begun: Begun::default(),
            tools: BTreeMap::new(),
        }
    }

    fn stamp(&self) -> String {
        stamp((self.now)())
    }

    /// Takes a message of the client. Returns what it means to a
    /// session, if anything.
    fn asked(&mut self, id: &str, asks: Asks, out: &mut Out) -> Option<FromClient> {
        match asks {
            Asks::ClientReady { version, .. } => {
                // A client that is not served in full is warned, as the
                // standard asks, and served.
                if let Some(warning) = not_in_full(version.as_deref()) {
                    out.refuse(id, warning);
                }
                let ready = BotReady {
                    version: rtvi::VERSION.to_owned(),
                    about: AboutBot {
                        library: "waveport".to_owned(),
                        library_version: env!("CARGO_PKG_VERSION").to_owned(),
                    },
                    capabilities: Capabilities {
                        audio_in: true,
                        audio_out: true,
                        ..Capabilities::default()
                    },
                };
                out.answer(id, Says::BotReady(ready));
                None
            }
            Asks::DisconnectBot => {
                // Its session ends with its goodbye, and it is told so
                // here: the session tells one that left nothing more.
                out.0.push(PEER_LEFT.to_owned());
                Some(FromClient::Bye)
            }
            Asks::SendText(sent) => {
                if !sent.options.run_immediately {
                    let why = "text that is not answered at once is not taken: a session has no \
                               turn that is not answered";
                    out.refuse(id, why);
                    return None;
                }
                if !sent.options.audio_response {
                    let why = "text that is not answered aloud is not taken: on a call the \
                               answer is spoken";
                    out.refuse(id, why);
                    return None;
                }
                let typed = FromClient::Text { text: sent.content };
                if let Err(refusal) = typed.check() {
                    out.refuse(id, refusal.to_string());
                    return None;
                }
                if let FromClient::Text { text } = &typed {
                    if self.typed.len() >= TYPED {
                        self.typed.pop_front();
                    }
                    self.typed.push_back((id.to_owned(), text.clone()));
                }
                Some(typed)
            }
            Asks::Dtmf(buttons) => {
                let pressed = keys(&buttons).map(|keys| FromClient::Dtmf { keys });
                let Some(pressed) = pressed.filter(|pressed| pressed.check().is_ok()) else {
                    let why = "the buttons are none, too many, or not keys of a telephone";
                    out.refuse(id, why);
                    return None;
                };
                Some(pressed)
            }
            Asks::Invalid(kind) => {
                out.refuse(
                    id,
                    format!("the data of this `{kind}` is not what a `{kind}` holds"),
                );
                None
            }
            Asks::Other(kind) => {
                out.refuse(id, not_served(&kind));
                None
            }
        }
    }

    /// The model and the audio of a response are done with, as far as
    /// the client was told they had begun.
    fn made(&mut self, out: &mut Out) {
        if take(&mut self.begun.thinking) {
            out.say(Says::BotLlmStopped);
        }
        if take(&mut self.begun.synthesising) {
            out.say(Says::BotTtsStopped);
        }
    }

    /// The audio of a response is being made, which the client is told
    /// once.
    fn synthesised(&mut self, out: &mut Out) {
        if !replace(&mut self.begun.synthesising, true) {
            out.say(Says::BotTtsStarted);
        }
    }

    /// Text of the response `generation` went out to be spoken.
    fn released(&mut self, generation: u32, text: &str, out: &mut Out) {
        // What has no words is nothing to show.
        if text.trim().is_empty() {
            return;
        }
        let id = self.segments;
        self.segments = self.segments.wrapping_add(1);
        let text = text.to_owned();
        // What an agent says that no model wrote, as it greets, is not
        // said to be a model's.
        if self.begun.thinking {
            out.say(Says::BotLlmText { text: text.clone() });
        }
        let progress = SpokenProgress {
            accumulated_text: String::new(),
            remaining_text: text.clone(),
        };
        let output = BotOutput::of(id, text.clone(), true, SpokenStatus::New, Some(progress));
        out.say(Says::BotOutput(output));
        // It went to the synthesiser, which has begun on the response
        // with it.
        self.synthesised(out);
        let response = self.responses.entry(generation).or_default();
        response.segments.push(Segment {
            id,
            chars: text.chars().count(),
            text,
            told: 0,
        });
        while self.responses.len() > RESPONSES {
            self.responses.pop_first();
        }
    }

    /// The response `generation` ended, with `text` heard of it.
    fn ended(&mut self, generation: u32, text: &str, truncated: bool, out: &mut Out) {
        if let Some(mut response) = self.responses.remove(&generation) {
            // What was heard of it since the client was last told: its
            // end, and all of one that was short.
            let (_, rest) = split(text, response.heard);
            if !rest.trim().is_empty() {
                let text = rest.to_owned();
                out.say(Says::BotTtsText { text });
            }
            response.heard = response.heard.max(text.chars().count());
            response.tell(out, !truncated);
        }
        // What was begun before it is heard no more.
        self.responses.retain(|older, _| *older > generation);
    }

    /// A participant's words, as an input of the kind `input_type`.
    fn input(&self, who: u16, text: &str, input_type: InputType, is_final: bool) -> UserInput {
        UserInput {
            text: text.to_owned(),
            input_type,
            timestamp: self.stamp(),
            is_final,
            user_id: Some(who.to_string()),
            msg_id: None,
        }
    }

    /// The same, as the deprecated message has it.
    fn transcription(input: &UserInput) -> Says<Value> {
        Says::UserTranscription(UserTranscription {
            text: input.text.clone(),
            user_id: input.user_id.clone().unwrap_or_default(),
            timestamp: input.timestamp.clone(),
            is_final: input.is_final,
        })
    }

    fn news(&mut self, news: &News, out: &mut Out) {
        match news {
            News::UserSpeaking { speaking: true, .. } => out.say(Says::UserStartedSpeaking),
            News::UserSpeaking {
                speaking: false, ..
            } => out.say(Says::UserStoppedSpeaking),
            News::Agent { activity, .. } => match activity {
                Activity::Thinking => {
                    // A response that is begun while another was being
                    // made takes its place.
                    self.made(out);
                    self.begun.thinking = true;
                    out.say(Says::BotLlmStarted);
                }
                Activity::Speaking => {
                    // Nothing is heard that was not made first.
                    self.synthesised(out);
                    if !replace(&mut self.begun.speaking, true) {
                        out.say(Says::BotStartedSpeaking);
                    }
                }
                Activity::Listening => {
                    self.made(out);
                    if take(&mut self.begun.speaking) {
                        out.say(Says::BotStoppedSpeaking);
                    }
                }
                Activity::Unknown => {}
            },
            News::Said { generation, text } => self.released(*generation, text, out),
            News::Hearing { who, text } => {
                if self.hearing.len() >= SPEAKERS && !self.hearing.contains_key(who) {
                    self.hearing.pop_first();
                }
                self.hearing.insert(*who, text.clone());
                let input = self.input(*who, text, InputType::Transcription, false);
                let deprecated = Self::transcription(&input);
                out.say(Says::UserInput(input));
                out.say(deprecated);
            }
            News::Unknown => {}
        }
    }

    fn fact(&mut self, fact: &Fact, out: &mut Out) {
        match fact {
            Fact::TurnCommitted { who, text, .. } => {
                self.hearing.remove(who);
                let sent = |(_, typed): &(String, String)| typed.trim() == text.trim();
                let typed = self.typed.iter().position(sent);
                let msg_id = typed.and_then(|at| self.typed.remove(at)).map(|(id, _)| id);
                let kind = if msg_id.is_some() {
                    InputType::Chat
                } else {
                    InputType::Transcription
                };
                let mut input = self.input(*who, text, kind, true);
                input.msg_id = msg_id;
                let deprecated = Self::transcription(&input);
                out.say(Says::UserInput(input));
                if kind == InputType::Transcription {
                    out.say(deprecated);
                }
            }
            Fact::TurnDismissed { who, reason, .. } => {
                let words = self.hearing.remove(who);
                if let (Dismissal::Backchannel, Some(words)) = (reason, words) {
                    let input = self.input(*who, &words, InputType::Backchannel, true);
                    out.say(Says::UserInput(input));
                }
            }
            Fact::UtteranceProgress {
                generation, heard, ..
            } => {
                if !heard.trim().is_empty() {
                    let text = heard.clone();
                    out.say(Says::BotTtsText { text });
                }
                if let Some(response) = self.responses.get_mut(generation) {
                    response.heard = response.heard.saturating_add(heard.chars().count());
                    response.tell(out, false);
                }
            }
            Fact::UtteranceCommitted {
                generation,
                text,
                truncated,
                ..
            } => self.ended(*generation, text, *truncated, out),
            Fact::CutOff {
                cause: Cut::BargeIn | Cut::Interrupted,
            } => out.say(Says::BotInterrupted),
            Fact::ToolCalled {
                call, tool, args, ..
            } => {
                while self.tools.len() >= TOOLS {
                    self.tools.pop_first();
                }
                let (name, args) = (tool.clone(), args.clone());
                self.tools.insert(*call, Tool { name, args });
                let function_name = Some(tool.clone());
                out.say(Says::LlmFunctionCallStarted { function_name });
            }
            Fact::ToolSent { call, attempt: 0 } => {
                let tool = self.tools.get(call);
                // Arguments are an object, or they are not given.
                let arguments = tool
                    .and_then(|tool| tool.args.as_deref())
                    .map(answered)
                    .filter(Value::is_object);
                out.say(Says::LlmFunctionCallInProgress {
                    tool_call_id: call.to_string(),
                    function_name: tool.map(|tool| tool.name.clone()),
                    arguments,
                });
            }
            Fact::ToolFinished {
                call,
                outcome,
                result,
            } => {
                let tool = self.tools.remove(call);
                out.say(Says::LlmFunctionCallStopped {
                    tool_call_id: call.to_string(),
                    cancelled: *outcome == ToolOutcome::Cancelled,
                    function_name: tool.map(|tool| tool.name),
                    result: result.as_deref().map(answered),
                });
            }
            Fact::LifecycleChanged {
                to: Lifecycle::Closing | Lifecycle::Closed,
                reason: Some(CloseReason::Failure),
            } if !replace(&mut self.failed, true) => {
                let error = "the session could not go on, and is closing".to_owned();
                out.say(Says::Error { error, fatal: true });
            }
            _ => {}
        }
    }
}

impl Dialect for Gateway {
    fn channel(&self) -> &'static str {
        CHANNEL
    }

    fn heard(&mut self, said: &str) -> Meant {
        let mut out = Out::default();
        let means = match rtvi::read::<Json>(said) {
            // The transport's own: a ping, the state of a track.
            Written::Foreign => None,
            Written::Unreadable => {
                let error =
                    "a message of RTVI has a `type` and an `id`, and this has not".to_owned();
                out.say(Says::Error {
                    error,
                    fatal: false,
                });
                None
            }
            Written::Said(rtvi::FromClient { id, asks }) => self.asked(&id, asks, &mut out),
        };
        Meant {
            means,
            answers: out.0,
        }
    }

    fn told(&mut self, message: &FromServer) -> Vec<String> {
        let mut out = Out::default();
        match message {
            FromServer::Welcome { .. } => self.welcomed = true,
            FromServer::Mark { end: false, .. } => self.synthesised(&mut out),
            // The last audio of a response has been made, and so the
            // model had finished.
            FromServer::Mark { end: true, .. } => self.made(&mut out),
            FromServer::Fact { fact, .. } => self.fact(fact, &mut out),
            FromServer::Told(news) => self.news(news, &mut out),
            // One that was not welcomed is not served: what it is told
            // is why.
            FromServer::Error { message, .. } => out.say(Says::Error {
                error: message.clone(),
                fatal: !self.welcomed,
            }),
            // A call that another takes the place of is the client's
            // own doing: its peer has not left, and it is not told so on
            // a call it may not have let go of yet.
            FromServer::Bye {
                reason: ByeReason::Replaced,
            } => {}
            FromServer::Bye { .. } => out.0.push(PEER_LEFT.to_owned()),
            // What is to be discarded cannot be by this client, and
            // where a session is, is what a client of v1 keeps.
            _ => {}
        }
        out.0
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde_json::{Value, json};
    use waveport_protocol::client::v1::{
        Activity, ByeReason, CloseReason, Cut, Dismissal, ErrorCode, Fact, FromClient, FromServer,
        Lifecycle, MOST_KEYS, MOST_TEXT, News, ToolOutcome,
    };
    use waveport_transport_webrtc::{Dialect, Meant};

    use super::{Gateway, PEER_LEFT, RESPONSES, TYPED, stamp};

    /// Ten in the morning of 20 November 2026, and a second.
    fn then() -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(1_795_168_801_250)
    }

    const THEN: &str = "2026-11-20T10:00:01.250Z";

    fn gateway() -> Gateway {
        Gateway::at(then)
    }

    /// What was written, each as the JSON it is.
    fn read(written: &[String]) -> Vec<Value> {
        written
            .iter()
            .map(|text| serde_json::from_str(text).unwrap())
            .collect()
    }

    /// What kind each message is that was written.
    fn kinds(written: &[String]) -> Vec<String> {
        read(written)
            .iter()
            .map(|message| message["type"].as_str().unwrap().to_owned())
            .collect()
    }

    /// A message of a client, as it writes it.
    fn message(kind: &str, id: &str, data: &Value) -> String {
        json!({"label": "rtvi-ai", "type": kind, "id": id, "data": data}).to_string()
    }

    fn fact(fact: Fact) -> FromServer {
        FromServer::Fact { seq: 1, fact }
    }

    fn agent(activity: Activity, generation: u32) -> FromServer {
        FromServer::Told(News::Agent {
            agent: 0,
            generation: Some(generation),
            activity,
        })
    }

    fn said(generation: u32, text: &str) -> FromServer {
        let text = text.to_owned();
        FromServer::Told(News::Said { generation, text })
    }

    fn heard(generation: u32, heard: &str) -> FromServer {
        fact(Fact::UtteranceProgress {
            generation,
            agent: 0,
            heard: heard.to_owned(),
        })
    }

    fn committed(generation: u32, text: &str, truncated: bool) -> FromServer {
        fact(Fact::UtteranceCommitted {
            generation,
            agent: 0,
            text: text.to_owned(),
            truncated,
        })
    }

    const fn mark(generation: u32, end: bool) -> FromServer {
        FromServer::Mark {
            run: 0,
            generation,
            end,
            ts: None,
        }
    }

    /// What each `bot-output` among `written` says of its stretch: the
    /// stretch, where it is, the text, what was heard and what was not.
    fn outputs(written: &[String]) -> Vec<(u64, String, String, String, String)> {
        let text = |value: &Value| value.as_str().unwrap().to_owned();
        read(written)
            .iter()
            .filter(|message| message["type"] == "bot-output")
            .map(|message| {
                let data = &message["data"];
                assert_eq!(data["will_be_spoken"], true);
                for name in ["output_type", "text_type", "aggregated_by"] {
                    assert_eq!(data[name], "sentence");
                }
                let progress = &data["spoken_progress"];
                (
                    data["segment_id"].as_u64().unwrap(),
                    text(&data["spoken_status"]),
                    text(&data["text"]),
                    text(&progress["accumulated_text"]),
                    text(&progress["remaining_text"]),
                )
            })
            .collect()
    }

    #[test]
    fn a_client_that_is_ready_is_answered_by_its_id_and_warned_where_it_is_not_served_in_full() {
        let mut gateway = gateway();
        // The stock client, as the spike recorded it.
        let ready = r#"{"label":"rtvi-ai","type":"client-ready","data":{"version":"2.3.0","about":{"library":"@pipecat-ai/client-js","library_version":"1.16.0","platform":"Linux"}},"id":"0dca44e2"}"#;
        let Meant { means, answers } = gateway.heard(ready);
        // It means nothing to a session.
        assert_eq!(means, None);
        let answers = read(&answers);
        assert_eq!(answers.len(), 1, "{answers:?}");
        assert_eq!(
            answers[0],
            json!({
                "label": "rtvi-ai",
                "type": "bot-ready",
                "id": "0dca44e2",
                "data": {
                    "version": "2.3.0",
                    "about": {"library": "waveport", "library_version": env!("CARGO_PKG_VERSION")},
                    "capabilities": {
                        "audio_in": true, "audio_out": true, "video_in": false,
                        "screen_in": false, "video_out": false, "metrics": false,
                    },
                },
            })
        );
        // Any version of the same major one is served in full.
        for version in ["2.0.0", "2.3.1", "2.17.4"] {
            let ready = message("client-ready", "r", &json!({"version": version}));
            assert_eq!(
                kinds(&gateway.heard(&ready).answers),
                ["bot-ready"],
                "{version}"
            );
        }
        // Any other is warned by the id of what it said, and then
        // answered: the call goes on. So is one that says none.
        for (data, about) in [
            (json!({"version": "1.4.0"}), "version 1"),
            (json!({"version": "3.0.0"}), "not compatible"),
            (json!({"version": "0.9.0"}), "not compatible"),
            (json!({"version": "2.3"}), "is none"),
            (json!({"version": "2.3.0.1"}), "is none"),
            (json!({"version": "two"}), "is none"),
            (json!({"version": "-2.3.0"}), "is none"),
            (json!({}), "does not say"),
            (Value::Null, "does not say"),
        ] {
            let ready = message("client-ready", "r7", &data);
            let Meant { means, answers } = gateway.heard(&ready);
            assert_eq!(means, None);
            assert_eq!(kinds(&answers), ["error-response", "bot-ready"], "{data}");
            let answers = read(&answers);
            assert_eq!(answers[0]["id"], "r7");
            assert_eq!(answers[1]["id"], "r7");
            let warning = answers[0]["data"]["error"].as_str().unwrap();
            assert!(warning.contains(about), "{data}: {warning}");
            // It is spoken to in the version that is spoken here.
            assert_eq!(answers[1]["data"]["version"], "2.3.0");
        }
    }

    #[test]
    fn what_a_client_asks_of_a_session_is_what_a_client_of_v1_would_say() {
        let mut gateway = gateway();
        let text = "what time is it".to_owned();
        for data in [
            json!({"content": text}),
            json!({"content": text, "options": {"run_immediately": true, "audio_response": true}}),
        ] {
            let Meant { means, answers } = gateway.heard(&message("send-text", "c1", &data));
            assert_eq!(means, Some(FromClient::Text { text: text.clone() }));
            assert_eq!(answers, Vec::<String>::new());
        }
        let buttons = json!({"buttons": ["1", "2", "#", "*", "0", "A"]});
        let Meant { means, answers } = gateway.heard(&message("dtmf", "d1", &buttons));
        let keys = "12#*0A".to_owned();
        assert_eq!(means, Some(FromClient::Dtmf { keys }));
        assert_eq!(answers, Vec::<String>::new());
        // As many keys as one message of v1 may hold, and no more.
        let most = vec!["5"; MOST_KEYS];
        let pressed = gateway.heard(&message("dtmf", "d2", &json!({"buttons": most})));
        assert_eq!(
            pressed.means,
            Some(FromClient::Dtmf {
                keys: "5".repeat(MOST_KEYS)
            })
        );
        for data in [json!({}), Value::Null, json!("whatever")] {
            let Meant { means, answers } = gateway.heard(&message("disconnect-bot", "b1", &data));
            assert_eq!(means, Some(FromClient::Bye));
            // It is told that its peer left, so that it does not take
            // the end of its call for a network that failed.
            assert_eq!(answers, [PEER_LEFT]);
        }
    }

    #[test]
    fn what_is_not_acted_on_is_refused_by_its_id_and_means_nothing() {
        let mut gateway = gateway();
        let long = "x".repeat(MOST_TEXT + 1);
        let many = vec!["1"; MOST_KEYS + 1];
        for (kind, data, why) in [
            // Text a session cannot take as RTVI means it.
            (
                "send-text",
                json!({"content": "note this", "options": {"run_immediately": false}}),
                "not answered at once",
            ),
            (
                "send-text",
                json!({"content": "in writing", "options": {"audio_response": false}}),
                "not answered aloud",
            ),
            ("send-text", json!({"content": "  "}), "empty or too long"),
            ("send-text", json!({"content": long}), "empty or too long"),
            (
                "send-text",
                json!({"content": 7}),
                "is not what a `send-text` holds",
            ),
            ("send-text", Value::Null, "is not what a `send-text` holds"),
            // Keys that are none.
            ("dtmf", json!({"buttons": []}), "not keys of a telephone"),
            (
                "dtmf",
                json!({"buttons": ["1", "two"]}),
                "not keys of a telephone",
            ),
            (
                "dtmf",
                json!({"buttons": ["12"]}),
                "not keys of a telephone",
            ),
            ("dtmf", json!({"buttons": [""]}), "not keys of a telephone"),
            ("dtmf", json!({"buttons": ["x"]}), "not keys of a telephone"),
            ("dtmf", json!({"buttons": many}), "too many"),
            ("dtmf", json!({"button": "1"}), "is not what a `dtmf` holds"),
            // What the standard has and a session does not do.
            (
                "send-file",
                json!({"content": "look", "file": {}}),
                "takes no files",
            ),
            (
                "llm-function-call-result",
                json!({"tool_call_id": "7"}),
                "run by the server",
            ),
            (
                "client-message",
                json!({"t": "anything"}),
                "nothing here for an application",
            ),
            ("ui-event", json!({"event": "click"}), "UI Agent Protocol"),
            ("ui-snapshot", json!({"tree": {}}), "UI Agent Protocol"),
            (
                "ui-cancel-job-group",
                json!({"job_id": "j"}),
                "UI Agent Protocol",
            ),
            ("append-to-context", json!({"content": "x"}), "deprecated"),
            ("raw-audio", json!({"audio": ""}), "audio of the call"),
            ("raw-audio-batch", json!({"batch": []}), "audio of the call"),
            // And what nobody has.
            ("wpc1_madeup", json!({"wpc1_madeup": 1}), "not known here"),
        ] {
            let Meant { means, answers } = gateway.heard(&message(kind, "m-3", &data));
            assert_eq!(means, None, "{kind} {data}");
            let answers = read(&answers);
            assert_eq!(answers.len(), 1, "{kind} {data}: {answers:?}");
            assert_eq!(answers[0]["type"], "error-response", "{kind} {data}");
            assert_eq!(answers[0]["id"], "m-3", "{kind} {data}");
            let error = answers[0]["data"]["error"].as_str().unwrap();
            assert!(error.contains(why), "{kind} {data}: {error}");
            // What it was is not said back.
            assert!(!error.contains("madeup"));
        }
        // Text that was refused is not waited for as a turn.
        assert!(gateway.typed.is_empty());
    }

    #[test]
    fn what_is_not_rtvi_is_passed_over_and_what_cannot_be_answered_by_an_id_is_an_error() {
        let mut gateway = gateway();
        // The transport's own, and whatever else is written there.
        for foreign in [
            "ping: 1794862249583",
            r#"{"type":"signalling","message":{"type":"trackStatus","receiver_index":0,"enabled":true}}"#,
            r#"{"label":"another","type":"client-ready","id":"x"}"#,
            r#"{"type":"hello","v":1}"#,
            "",
            "{",
            "[]",
        ] {
            assert_eq!(gateway.heard(foreign), Meant::default(), "{foreign}");
        }
        // Labelled as RTVI, with no id to answer by.
        for unreadable in [
            r#"{"label":"rtvi-ai","type":"send-text","data":{"content":"wpc1_madeup"}}"#,
            r#"{"label":"rtvi-ai","id":"x"}"#,
            r#"{"label":"rtvi-ai","type":"disconnect-bot","id":7}"#,
        ] {
            let Meant { means, answers } = gateway.heard(unreadable);
            // Not even a goodbye is taken from it.
            assert_eq!(means, None, "{unreadable}");
            let answers = read(&answers);
            assert_eq!(answers.len(), 1);
            assert_eq!(answers[0]["type"], "error");
            assert_eq!(answers[0]["data"]["fatal"], false);
            assert_eq!(answers[0].get("id"), None);
            assert!(!answers[0].to_string().contains("madeup"));
        }
    }

    #[test]
    fn a_response_is_told_from_the_model_to_the_last_word_heard() {
        let mut gateway = gateway();
        let mut told = Vec::new();
        let whole = "It is ten o'clock. Anything else?";
        for message in [
            agent(Activity::Thinking, 4),
            said(4, "It is ten o'clock. "),
            said(4, "Anything else?"),
            agent(Activity::Speaking, 4),
            mark(4, false),
            heard(4, "It is ten "),
            // It is said to speak once, however often the session says
            // so.
            agent(Activity::Speaking, 4),
            heard(4, "o'clock. Anything "),
            mark(4, true),
            heard(4, "else?"),
            committed(4, whole, false),
            agent(Activity::Listening, 4),
        ] {
            told.extend(gateway.told(&message));
        }
        assert_eq!(
            kinds(&told),
            [
                "bot-llm-started",
                "bot-llm-text",
                "bot-output",
                "bot-tts-started",
                "bot-llm-text",
                "bot-output",
                "bot-started-speaking",
                "bot-tts-text",
                "bot-output",
                "bot-tts-text",
                "bot-output",
                "bot-output",
                "bot-llm-stopped",
                "bot-tts-stopped",
                "bot-tts-text",
                "bot-output",
                "bot-stopped-speaking",
            ]
        );
        let own = |text: &str| text.to_owned();
        let output = |id: u64, status: &str, text: &str, heard: &str, rest: &str| {
            (id, own(status), own(text), own(heard), own(rest))
        };
        assert_eq!(
            outputs(&told),
            [
                // Each stretch as it goes out, with nothing heard of it.
                output(0, "new", "It is ten o'clock. ", "", "It is ten o'clock. "),
                output(1, "new", "Anything else?", "", "Anything else?"),
                // What is heard, laid over the stretches: the text is
                // what is new, and what was heard is all that was.
                output(0, "in-progress", "It is ten ", "It is ten ", "o'clock. "),
                output(0, "completed", "o'clock. ", "It is ten o'clock. ", ""),
                output(1, "in-progress", "Anything ", "Anything ", "else?"),
                output(1, "completed", "else?", "Anything else?", ""),
            ]
        );
        // The deprecated messages carry the text as it went out and as
        // it was heard.
        let texts = |kind: &str| -> Vec<String> {
            read(&told)
                .iter()
                .filter(|message| message["type"] == kind)
                .map(|message| message["data"]["text"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(
            texts("bot-llm-text"),
            ["It is ten o'clock. ", "Anything else?"]
        );
        assert_eq!(
            texts("bot-tts-text"),
            ["It is ten ", "o'clock. Anything ", "else?"]
        );
        // Nothing is kept of a response that is over.
        assert!(gateway.responses.is_empty());
        // And nothing answers anything: no message has an id.
        assert!(
            read(&told)
                .iter()
                .all(|message| message.get("id").is_none())
        );
    }

    #[test]
    fn what_an_agent_says_that_no_model_wrote_is_not_said_to_be_a_model_s() {
        let mut gateway = gateway();
        let mut told = Vec::new();
        // A greeting: the agent speaks, and no model was at work.
        for message in [
            said(1, "Good morning."),
            agent(Activity::Speaking, 1),
            mark(1, false),
            heard(1, "Good morning."),
            mark(1, true),
            committed(1, "Good morning.", false),
            agent(Activity::Listening, 1),
        ] {
            told.extend(gateway.told(&message));
        }
        assert_eq!(
            kinds(&told),
            [
                "bot-output",
                "bot-tts-started",
                "bot-started-speaking",
                "bot-tts-text",
                "bot-output",
                "bot-tts-stopped",
                "bot-stopped-speaking",
            ]
        );
        // An agent that is heard before anything was said of what it
        // says is said to be made audible first, and once.
        let told = gateway.told(&agent(Activity::Speaking, 2));
        assert_eq!(kinds(&told), ["bot-tts-started", "bot-started-speaking"]);
        let told = gateway.told(&mark(2, false));
        assert_eq!(kinds(&told), Vec::<String>::new());
        let told = gateway.told(&agent(Activity::Listening, 2));
        assert_eq!(kinds(&told), ["bot-tts-stopped", "bot-stopped-speaking"]);
        // The response after it is a model's, and is said to be.
        let mut told = gateway.told(&agent(Activity::Thinking, 2));
        told.extend(gateway.told(&said(2, "It is ten.")));
        assert_eq!(
            kinds(&told),
            [
                "bot-llm-started",
                "bot-llm-text",
                "bot-output",
                "bot-tts-started"
            ]
        );
    }

    #[test]
    fn what_was_heard_is_counted_in_characters_and_a_response_heard_to_its_end_is_completed() {
        let mut gateway = gateway();
        gateway.told(&said(1, "Zoë naïve — "));
        gateway.told(&said(1, "日本語。"));
        // What has no words is no stretch.
        assert_eq!(gateway.told(&said(1, "  ")), Vec::<String>::new());
        let own = |text: &str| text.to_owned();
        // Seven characters, which are more bytes than that.
        let told = gateway.told(&heard(1, "Zoë naï"));
        assert_eq!(
            outputs(&told),
            [(
                0,
                own("in-progress"),
                own("Zoë naï"),
                own("Zoë naï"),
                own("ve — ")
            )]
        );
        // What is heard next ends one stretch and begins the other.
        let told = gateway.told(&heard(1, "ve — 日本"));
        assert_eq!(
            outputs(&told),
            [
                (
                    0,
                    own("completed"),
                    own("ve — "),
                    own("Zoë naïve — "),
                    own("")
                ),
                (1, own("in-progress"), own("日本"), own("日本"), own("語。")),
            ]
        );
        // Words that add nothing to what was told of a stretch say
        // nothing of it.
        assert_eq!(kinds(&gateway.told(&heard(1, ""))), Vec::<String>::new());
        // The response was heard to its end, though less was counted:
        // every stretch is completed.
        let told = gateway.told(&committed(1, "Zoë naïve — 日本", false));
        assert_eq!(
            outputs(&told),
            [(1, own("completed"), own("語。"), own("日本語。"), own(""))]
        );

        // One that was cut off stays where it was heard to.
        gateway.told(&said(2, "One two three."));
        gateway.told(&heard(2, "One "));
        let told = gateway.told(&committed(2, "One ", true));
        assert_eq!(told, Vec::<String>::new());
        // More was heard than went out, as the count has it: no stretch
        // is told past its end.
        gateway.told(&said(3, "Short."));
        let told = gateway.told(&heard(3, "Short. And more than that."));
        assert_eq!(
            outputs(&told),
            [(3, own("completed"), own("Short."), own("Short."), own(""))]
        );
        assert_eq!(gateway.told(&heard(3, " And more.")).len(), 1);
        // What is heard of a response nobody was told of is its words
        // and no stretch.
        assert_eq!(kinds(&gateway.told(&heard(9, "Who?"))), ["bot-tts-text"]);
    }

    #[test]
    fn what_was_heard_of_a_response_since_the_client_was_last_told_is_told_as_it_ends() {
        let mut gateway = gateway();
        let own = |text: &str| text.to_owned();
        let texts = |told: &[String]| -> Vec<String> {
            read(told)
                .iter()
                .filter(|message| message["type"] == "bot-tts-text")
                .map(|message| message["data"]["text"].as_str().unwrap().to_owned())
                .collect()
        };
        // One that was short: nothing was said of it before it ended.
        gateway.told(&said(1, "Good morning."));
        let told = gateway.told(&committed(1, "Good morning.", false));
        assert_eq!(kinds(&told), ["bot-tts-text", "bot-output"]);
        assert_eq!(texts(&told), ["Good morning."]);
        let whole = (
            0,
            own("completed"),
            own("Good morning."),
            own("Good morning."),
            own(""),
        );
        assert_eq!(outputs(&told), [whole]);
        // One that was cut off: what was heard of it is where it was
        // cut, and no further.
        gateway.told(&said(2, "One two three four."));
        let told = gateway.told(&heard(2, "One "));
        assert_eq!(texts(&told), ["One "]);
        let told = gateway.told(&committed(2, "One two", true));
        assert_eq!(kinds(&told), ["bot-tts-text", "bot-output"]);
        assert_eq!(texts(&told), ["two"]);
        let cut = (
            1,
            own("in-progress"),
            own("two"),
            own("One two"),
            own(" three four."),
        );
        assert_eq!(outputs(&told), [cut]);
        // One that was told to its end is told nothing twice.
        gateway.told(&said(3, "Yes."));
        gateway.told(&heard(3, "Yes."));
        let told = gateway.told(&committed(3, "Yes.", false));
        assert_eq!(kinds(&told), Vec::<String>::new());
        // And one the client was told nothing of, which began before
        // it came, is not told from its middle.
        let told = gateway.told(&committed(9, "It was said before.", false));
        assert_eq!(kinds(&told), Vec::<String>::new());
    }

    #[test]
    fn a_response_that_is_cut_off_is_interrupted_and_what_was_begun_of_it_is_ended() {
        for (cause, interrupted) in [
            (Cut::BargeIn, true),
            (Cut::Interrupted, true),
            (Cut::Handoff, false),
            (Cut::Superseded, false),
            (Cut::Failure, false),
            (Cut::Recovery, false),
            (Cut::Closing, false),
        ] {
            let mut gateway = gateway();
            for message in [
                agent(Activity::Thinking, 1),
                said(1, "It is ten o'clock."),
                mark(1, false),
                agent(Activity::Speaking, 1),
                heard(1, "It is "),
            ] {
                gateway.told(&message);
            }
            let told = gateway.told(&fact(Fact::CutOff { cause }));
            let expected: &[&str] = if interrupted {
                &["bot-interrupted"]
            } else {
                &[]
            };
            assert_eq!(kinds(&told), expected, "{cause:?}");
            assert_eq!(
                gateway.told(&committed(1, "It is ", true)),
                Vec::<String>::new()
            );
            // No mark ends audio that was cut: what was begun is ended
            // when the agent listens again, each thing once.
            let told = gateway.told(&agent(Activity::Listening, 1));
            assert_eq!(
                kinds(&told),
                ["bot-llm-stopped", "bot-tts-stopped", "bot-stopped-speaking"]
            );
            let again = gateway.told(&agent(Activity::Listening, 1));
            assert_eq!(again, Vec::<String>::new());
        }
        // A response that is begun while another is being made takes
        // its place: the first is ended before the second begins.
        let mut gateway = gateway();
        gateway.told(&agent(Activity::Thinking, 1));
        gateway.told(&mark(1, false));
        let told = gateway.told(&agent(Activity::Thinking, 2));
        assert_eq!(
            kinds(&told),
            ["bot-llm-stopped", "bot-tts-stopped", "bot-llm-started"]
        );
        // What is discarded for a client of v1 says nothing here.
        assert_eq!(
            gateway.told(&FromServer::Clear { run: 0 }),
            Vec::<String>::new()
        );
    }

    #[test]
    fn what_a_participant_says_and_types_is_told_as_input() {
        let mut gateway = gateway();
        let speaking = |speaking| {
            FromServer::Told(News::UserSpeaking {
                who: 3,
                turn: 1,
                speaking,
            })
        };
        assert_eq!(
            kinds(&gateway.told(&speaking(true))),
            ["user-started-speaking"]
        );
        let hearing = FromServer::Told(News::Hearing {
            who: 3,
            text: "what time".to_owned(),
        });
        let told = read(&gateway.told(&hearing));
        let interim = json!({
            "text": "what time", "input_type": "transcription", "timestamp": THEN,
            "final": false, "user_id": "3",
        });
        assert_eq!(
            told[0],
            json!({"label": "rtvi-ai", "type": "user-input", "data": interim})
        );
        let deprecated = json!({
            "text": "what time", "user_id": "3", "timestamp": THEN, "final": false,
        });
        assert_eq!(
            told[1],
            json!({"label": "rtvi-ai", "type": "user-transcription", "data": deprecated})
        );
        assert_eq!(told.len(), 2);
        assert_eq!(
            kinds(&gateway.told(&speaking(false))),
            ["user-stopped-speaking"]
        );
        let turn = |text: &str| {
            fact(Fact::TurnCommitted {
                turn: 1,
                who: 3,
                text: text.to_owned(),
                addressed: true,
            })
        };
        let told = read(&gateway.told(&turn("what time is it")));
        assert_eq!(told.len(), 2);
        assert_eq!(told[0]["type"], "user-input");
        assert_eq!(told[0]["data"]["input_type"], "transcription");
        assert_eq!(told[0]["data"]["final"], true);
        assert_eq!(told[0]["data"].get("msg_id"), None);
        assert_eq!(told[1]["type"], "user-transcription");
        assert_eq!(told[1]["data"]["final"], true);
        assert_eq!(told[1]["data"]["text"], "what time is it");

        // What the client sent as text is told as that, by the id of
        // its message, once: and not as something that was recognised.
        let typed = message("send-text", "c1", &json!({"content": "and tomorrow?"}));
        assert!(gateway.heard(&typed).means.is_some());
        let told = read(&gateway.told(&turn("and tomorrow?")));
        assert_eq!(told.len(), 1, "{told:?}");
        assert_eq!(told[0]["type"], "user-input");
        assert_eq!(told[0]["data"]["input_type"], "chat");
        assert_eq!(told[0]["data"]["msg_id"], "c1");
        assert_eq!(told[0]["data"]["final"], true);
        // The same words, said, are speech.
        let told = read(&gateway.told(&turn("and tomorrow?")));
        assert_eq!(told[0]["data"]["input_type"], "transcription");
        assert_eq!(told.len(), 2);
    }

    #[test]
    fn a_noise_of_agreement_is_told_with_the_words_that_were_recognised_of_it() {
        let dismissed = |reason| {
            fact(Fact::TurnDismissed {
                turn: 2,
                who: 3,
                reason,
            })
        };
        let hearing = |text: &str| {
            FromServer::Told(News::Hearing {
                who: 3,
                text: text.to_owned(),
            })
        };
        let mut gateway = gateway();
        gateway.told(&hearing("mhm"));
        let told = read(&gateway.told(&dismissed(Dismissal::Backchannel)));
        let data = json!({
            "text": "mhm", "input_type": "backchannel", "timestamp": THEN, "final": true,
            "user_id": "3",
        });
        assert_eq!(
            told,
            [json!({"label": "rtvi-ai", "type": "user-input", "data": data})]
        );
        // Once: the words are those of that noise and of no other.
        assert_eq!(
            gateway.told(&dismissed(Dismissal::Backchannel)),
            Vec::<String>::new()
        );
        // What was dismissed as anything else is told as nothing, and
        // its words are not those of what is dismissed next.
        for reason in [Dismissal::Noise, Dismissal::Empty, Dismissal::Closed] {
            gateway.told(&hearing("uh"));
            assert_eq!(gateway.told(&dismissed(reason)), Vec::<String>::new());
            assert_eq!(
                gateway.told(&dismissed(Dismissal::Backchannel)),
                Vec::<String>::new()
            );
        }
        // Nor are the words of a turn that was committed.
        gateway.told(&hearing("yes"));
        gateway.told(&fact(Fact::TurnCommitted {
            turn: 3,
            who: 3,
            text: "yes".to_owned(),
            addressed: true,
        }));
        assert_eq!(
            gateway.told(&dismissed(Dismissal::Backchannel)),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_tool_call_is_told_by_its_name_and_its_number_and_shows_only_what_it_was_given() {
        let mut gateway = gateway();
        let called = |call, args: Option<&str>| {
            fact(Fact::ToolCalled {
                call,
                tool: "clock".to_owned(),
                generation: 1,
                args: args.map(str::to_owned),
            })
        };
        let sent = |call, attempt| fact(Fact::ToolSent { call, attempt });
        let finished = |call, outcome, result: Option<&str>| {
            fact(Fact::ToolFinished {
                call,
                outcome,
                result: result.map(str::to_owned),
            })
        };
        // As a client is shown it: no arguments, and no answer.
        let told = read(&gateway.told(&called(7, None)));
        let data = json!({"function_name": "clock"});
        assert_eq!(
            told,
            [json!({"label": "rtvi-ai", "type": "llm-function-call-started", "data": data})]
        );
        let told = read(&gateway.told(&sent(7, 0)));
        let data = json!({"tool_call_id": "7", "function_name": "clock"});
        assert_eq!(
            told,
            [json!({"label": "rtvi-ai", "type": "llm-function-call-in-progress", "data": data})]
        );
        // A call that is sent again is the call that is in progress.
        assert_eq!(gateway.told(&sent(7, 1)), Vec::<String>::new());
        let told = read(&gateway.told(&finished(7, ToolOutcome::Completed, None)));
        let data = json!({"tool_call_id": "7", "cancelled": false, "function_name": "clock"});
        assert_eq!(
            told,
            [json!({"label": "rtvi-ai", "type": "llm-function-call-stopped", "data": data})]
        );
        assert!(gateway.tools.is_empty());

        // Where what a tool was given and what it answered may leave
        // the server, they are given: arguments as the object they are,
        // an answer as JSON where it is that and as text where it is
        // not.
        gateway.told(&called(8, Some(r#"{"zone":"UTC"}"#)));
        let told = read(&gateway.told(&sent(8, 0)));
        assert_eq!(told[0]["data"]["arguments"], json!({"zone": "UTC"}));
        let told = read(&gateway.told(&finished(8, ToolOutcome::Completed, Some(r#"{"at":10}"#))));
        assert_eq!(told[0]["data"]["result"], json!({"at": 10}));
        gateway.told(&called(9, Some("not an object")));
        let told = read(&gateway.told(&sent(9, 0)));
        assert_eq!(told[0]["data"].get("arguments"), None);
        let told = read(&gateway.told(&finished(9, ToolOutcome::Cancelled, Some("ten o'clock"))));
        assert_eq!(told[0]["data"]["result"], "ten o'clock");
        assert_eq!(told[0]["data"]["cancelled"], true);
        // Only a call that was cancelled is said to be.
        for outcome in [
            ToolOutcome::Failed,
            ToolOutcome::InDoubt,
            ToolOutcome::Unknown,
        ] {
            let told = read(&gateway.told(&finished(10, outcome, None)));
            assert_eq!(told[0]["data"]["cancelled"], false, "{outcome:?}");
            // One the client was never told the name of has none.
            assert_eq!(told[0]["data"].get("function_name"), None);
        }
    }

    #[test]
    fn an_error_is_fatal_for_whoever_was_not_welcomed_and_the_end_is_told_to_the_transport() {
        let error = || FromServer::Error {
            code: ErrorCode::Busy,
            message: "not now".to_owned(),
        };
        let welcome = FromServer::Welcome {
            v: 1,
            session: "s-7".to_owned(),
            life: 1,
            resumed: false,
            resume_token: Some("wpr1_madeup".to_owned()),
            audio: None,
            features: Vec::new(),
            next_seq: 1,
            grace_ms: None,
        };
        let mut gateway = gateway();
        // One that is told why it is not served is not served.
        let told = read(&gateway.told(&error()));
        let data = json!({"error": "not now", "fatal": true});
        assert_eq!(
            told,
            [json!({"label": "rtvi-ai", "type": "error", "data": data})]
        );
        // A welcome is v1's, and nothing of it is said: not the token a
        // client of v1 would come back with.
        assert_eq!(gateway.told(&welcome), Vec::<String>::new());
        let told = read(&gateway.told(&error()));
        assert_eq!(told[0]["data"]["fatal"], false);

        // A session that is closing because it failed is an error that
        // is fatal, said once. One that closes for anything else is not.
        let closing = |to, reason| fact(Fact::LifecycleChanged { to, reason });
        for reason in [
            Some(CloseReason::Requested),
            Some(CloseReason::Abandoned),
            None,
        ] {
            let told = gateway.told(&closing(Lifecycle::Closing, reason));
            assert_eq!(told, Vec::<String>::new(), "{reason:?}");
        }
        let failure = Some(CloseReason::Failure);
        assert_eq!(
            gateway.told(&closing(Lifecycle::Running, failure)),
            Vec::<String>::new()
        );
        let told = read(&gateway.told(&closing(Lifecycle::Closing, failure)));
        assert_eq!(told.len(), 1);
        assert_eq!(told[0]["type"], "error");
        assert_eq!(told[0]["data"]["fatal"], true);
        assert_eq!(
            gateway.told(&closing(Lifecycle::Closed, failure)),
            Vec::<String>::new()
        );

        // Goodbye: the client's peer has left, and it calls no more.
        for reason in [
            ByeReason::Ended,
            ByeReason::GoingAway,
            ByeReason::Refused,
            ByeReason::Unknown,
        ] {
            assert_eq!(gateway.told(&FromServer::Bye { reason }), [PEER_LEFT]);
        }
        // But for the call that another took the place of: that is the
        // client calling again, and its peer is where it was.
        let reason = ByeReason::Replaced;
        assert_eq!(
            gateway.told(&FromServer::Bye { reason }),
            Vec::<String>::new()
        );
        // What only a client of v1 keeps count with is not told.
        assert_eq!(
            gateway.told(&FromServer::Gap { resumes: 5 }),
            Vec::<String>::new()
        );
        for silent in [
            Fact::ParticipantJoined { who: 3 },
            Fact::ParticipantLeft { who: 3 },
            Fact::AgentActivated { agent: 1 },
            Fact::KeysCommitted {
                turn: 1,
                who: 3,
                keys: "12".to_owned(),
            },
            Fact::Unknown,
        ] {
            assert_eq!(gateway.told(&fact(silent)), Vec::<String>::new());
        }
    }

    #[test]
    fn what_is_kept_count_of_does_not_grow_with_the_call() {
        let mut gateway = gateway();
        // Responses that are begun and never said to be over.
        for generation in 0..100 {
            gateway.told(&said(generation, "Something."));
        }
        assert_eq!(gateway.responses.len(), RESPONSES);
        // The newest are the ones that are kept.
        assert!(gateway.responses.contains_key(&99));
        // A response that is over takes those before it with it.
        gateway.told(&committed(98, "Something.", false));
        assert_eq!(gateway.responses.keys().collect::<Vec<_>>(), [&99]);
        // Text that is sent and never becomes a turn.
        for n in 0..100 {
            let typed = message(
                "send-text",
                &format!("c{n}"),
                &json!({"content": format!("text {n}")}),
            );
            assert!(gateway.heard(&typed).means.is_some());
        }
        assert_eq!(gateway.typed.len(), TYPED);
        assert_eq!(gateway.typed.back().unwrap().0, "c99");
        // Tool calls that never end, and speakers that never stop.
        for call in 0..100 {
            gateway.told(&fact(Fact::ToolCalled {
                call,
                tool: "clock".to_owned(),
                generation: 1,
                args: None,
            }));
        }
        assert!(gateway.tools.len() <= super::TOOLS);
        assert!(gateway.tools.contains_key(&99));
        for who in 0..100 {
            let text = "mhm".to_owned();
            gateway.told(&FromServer::Told(News::Hearing { who, text }));
        }
        assert!(gateway.hearing.len() <= super::SPEAKERS);
        assert!(gateway.hearing.contains_key(&99));
    }

    #[test]
    fn the_time_of_day_is_written_as_iso_8601_says_it() {
        let at = |seconds: u64, millis: u64| {
            stamp(UNIX_EPOCH + Duration::from_millis(seconds * 1_000 + millis))
        };
        assert_eq!(at(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(at(86_399, 999), "1970-01-01T23:59:59.999Z");
        assert_eq!(at(68_169_600, 0), "1972-02-29T00:00:00.000Z");
        // A century that has a leap day, and the days around it.
        assert_eq!(at(951_782_399, 0), "2000-02-28T23:59:59.000Z");
        assert_eq!(at(951_782_400, 0), "2000-02-29T00:00:00.000Z");
        assert_eq!(at(951_868_800, 7), "2000-03-01T00:00:00.007Z");
        assert_eq!(at(1_700_000_000, 0), "2023-11-14T22:13:20.000Z");
        assert_eq!(at(1_767_225_599, 0), "2025-12-31T23:59:59.000Z");
        assert_eq!(at(1_767_225_600, 0), "2026-01-01T00:00:00.000Z");
        // One that has none.
        assert_eq!(at(4_107_542_400, 0), "2100-03-01T00:00:00.000Z");
        assert_eq!(at(4_107_456_000, 0), "2100-02-28T00:00:00.000Z");
        assert_eq!(stamp(then()), THEN);
        // A clock that is before all of this says the beginning.
        assert_eq!(
            stamp(UNIX_EPOCH - Duration::from_secs(5)),
            "1970-01-01T00:00:00.000Z"
        );
    }
}
