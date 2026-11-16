//! The names of what is sent.
//!
//! The names that begin with `gen_ai.` are those of the OpenTelemetry
//! conventions for generative AI. They are written out here, and not
//! taken from the crate of constants, which marks them as deprecated
//! while the conventions are still moving. The names that begin with
//! `waveport.` are this project's own.

/// The scope everything is sent under.
pub const SCOPE: &str = "waveport";

/// Attribute: the session, as the conventions name a conversation.
pub const CONVERSATION: &str = "gen_ai.conversation.id";
/// Attribute: what a span is doing, in the conventions' words.
pub const OPERATION: &str = "gen_ai.operation.name";
/// The operation of a span that runs a tool.
pub const EXECUTE_TOOL: &str = "execute_tool";
/// Attribute: the name of a tool.
pub const TOOL_NAME: &str = "gen_ai.tool.name";
/// Attribute: the number of a tool call.
pub const TOOL_CALL: &str = "gen_ai.tool.call.id";

/// Attribute: the number of a turn within its session.
pub const TURN: &str = "waveport.turn.id";
/// Attribute: whether a turn was said to the agent.
pub const ADDRESSED: &str = "waveport.turn.addressed";
/// Attribute: what became of a turn, `committed` or why it was dismissed.
pub const TURN_OUTCOME: &str = "waveport.turn.outcome";
/// Attribute: how long the caller waited for the first sound of the
/// answer, in seconds.
pub const WAITED: &str = "waveport.turn.time_to_first_frame";
/// Attribute: the number of a response within its session.
pub const GENERATION: &str = "waveport.response.id";
/// Attribute: whether an utterance was cut off.
pub const TRUNCATED: &str = "waveport.utterance.truncated";
/// Attribute: how a tool call ended.
pub const TOOL_OUTCOME: &str = "waveport.tool.outcome";
/// Attribute: why a session closed.
pub const CLOSE_REASON: &str = "waveport.session.close_reason";

/// Span: a turn of a caller and the answer to it.
pub const SPAN_TURN: &str = "turn";
/// Span: one response of the model, from asking for it to its end.
pub const SPAN_RESPONSE: &str = "response";

/// Event: the final transcript of the turn arrived.
pub const EVENT_TRANSCRIPT: &str = "transcript_final";
/// Event: it was decided that the turn has ended.
pub const EVENT_TURN_ENDED: &str = "turn_ended";
/// Event: the model wrote the first of the response.
pub const EVENT_FIRST_TOKEN: &str = "first_token";
/// Event: the first text was given to speech synthesis.
pub const EVENT_FIRST_TEXT: &str = "first_text";
/// Event: speech synthesis delivered the first audio.
pub const EVENT_FIRST_AUDIO: &str = "first_audio";
/// Event: the first frame was handed to the transport.
pub const EVENT_FIRST_FRAME: &str = "first_frame";
/// Event: the response was lowered because someone spoke over it.
pub const EVENT_LOWERED: &str = "lowered";
/// Event: the response was stopped.
pub const EVENT_STOPPED: &str = "stopped";
/// Event: the tool call was allowed.
pub const EVENT_AUTHORIZED: &str = "authorized";
/// Event: the tool call was sent to its tool.
pub const EVENT_SENT: &str = "sent";
/// Event: the tool call came back.
pub const EVENT_BACK: &str = "back";

/// Histogram, seconds: final transcript to the decision that the turn
/// has ended.
pub const DECIDING: &str = "waveport.turn.decide.duration";
/// Histogram, seconds: the end of a turn to the first frame of the
/// answer handed to the transport.
pub const ANSWERING: &str = "waveport.turn.time_to_first_frame";
/// Histogram, seconds: asking the model to the first it wrote.
pub const THINKING: &str = "waveport.llm.time_to_first_token";
/// Histogram, seconds: the model's first word to the first text given
/// to speech synthesis.
pub const PHRASING: &str = "waveport.tts.time_to_first_text";
/// Histogram, seconds: the first text given to speech synthesis to its
/// first audio.
pub const VOICING: &str = "waveport.tts.time_to_first_audio";
/// Histogram, seconds: the first audio of a response to its first frame
/// handed to the transport.
pub const SENDING: &str = "waveport.playout.time_to_first_frame";
/// Histogram, seconds: lowering a response to stopping it.
pub const YIELDING: &str = "waveport.interruption.yield.duration";
/// Histogram, seconds: the model asking for a tool call to the call
/// being sent.
pub const CLEARING: &str = "waveport.tool.clear.duration";
/// Histogram, seconds: sending a tool call to its coming back.
pub const CALLING: &str = "waveport.tool.call.duration";

/// Counter: turns, by what became of them.
pub const TURNS: &str = "waveport.turns";
/// Counter: utterances of the agent, by whether they were cut off.
pub const UTTERANCES: &str = "waveport.utterances";
/// Counter: tool calls that ended, by tool and by how.
pub const TOOL_CALLS: &str = "waveport.tool.calls";
/// Up-and-down counter: sessions under way.
pub const SESSIONS: &str = "waveport.sessions.active";
/// Counter: sessions that closed, by why.
pub const SESSIONS_CLOSED: &str = "waveport.sessions.closed";
