//! Taking the next event of a port, as a future.
//!
//! The receiving halves of the stream ports are polled, which is what a
//! runtime wants and what a test does not want to write out each time.

use core::future::poll_fn;
use core::pin::Pin;

use waveport_ports::error::PortError;
use waveport_ports::llm::{LlmEvent, LlmRx};
use waveport_ports::stt::{SttEvent, SttRx};
use waveport_ports::transport::{Transport, TransportEvent};
use waveport_ports::tts::{TtsEvent, TtsRx};

/// The next event of a stream of speech recognition.
pub async fn next_stt(rx: &mut Pin<Box<dyn SttRx>>) -> Option<Result<SttEvent, PortError>> {
    poll_fn(|cx| rx.as_mut().poll_next(cx)).await
}

/// The next event of a response.
pub async fn next_llm(rx: &mut Pin<Box<dyn LlmRx>>) -> Option<Result<LlmEvent, PortError>> {
    poll_fn(|cx| rx.as_mut().poll_next(cx)).await
}

/// The next event of a stream of speech synthesis.
pub async fn next_tts(rx: &mut Pin<Box<dyn TtsRx>>) -> Option<Result<TtsEvent, PortError>> {
    poll_fn(|cx| rx.as_mut().poll_next(cx)).await
}

/// The next event of a transport.
pub async fn next_transport<T: Transport + ?Sized>(transport: &mut T) -> Option<TransportEvent> {
    poll_fn(|cx| transport.poll_event(cx)).await
}
