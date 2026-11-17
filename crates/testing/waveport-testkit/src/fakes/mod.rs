//! Fakes of the ports: parts that follow a script.
//!
//! Each fake says what it can do in the same capability types a real
//! adapter uses, and then does exactly that, so a session can be planned
//! for a fake as for a provider. What a fake is asked to do it writes
//! down, for the test to read afterwards.
//!
//! The fakes keep time with `tokio::time`. Under a runtime whose clock is
//! paused, which is how the tests here run, a session with them takes no
//! real time and is the same every time.

pub mod journal;
pub mod llm;
pub mod models;
pub mod stt;
pub mod tools;
pub mod transport;
pub mod tts;

use core::time::Duration;

pub use journal::MemoryJournal;
pub use llm::{Asked, LlmStep, Said, ScriptedLlm};
pub use models::{ByLength, LoudVad, ScriptedTurns, TurnLog};
pub use stt::{ScriptedStt, Spoken, SttLog, SttScript};
pub use tools::{Called, ScriptedTools};
pub use transport::{
    FakeTransport, FakeTransportBuilder, SentFrame, SentTone, TransportHandle, TransportLog,
};
pub use tts::{ScriptedTts, TtsLog, TtsScript};

/// Waits for `wait`, and not at all when there is nothing to wait for.
///
/// A timer of no length is not free on a clock that runs: it ends on the
/// next millisecond. A stand-in that is to answer at once would answer
/// up to a millisecond late every time it is asked, and a measurement
/// in real time would count that as the session's.
pub(crate) async fn pause(wait: Duration) {
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
}
