//! Language models behind an OpenAI-compatible chat completions API.
//!
//! Stability: experimental
//!
//! One request, `POST {base}/chat/completions`, and an answer that arrives
//! as server-sent events. The adapter has two dialects. [`Dialect::OpenAi`]
//! is the provider the API is named after. [`Dialect::Generic`] is every
//! server that speaks the same protocol, such as a model on this machine,
//! and is asked for nothing beyond what they all understand.
//!
//! What was learnt from the provider itself, and is held by recorded
//! answers in this crate's tests:
//!
//! - A model that reasons before it answers takes seconds to begin, and
//!   on this API such a model refuses tools unless it is told not to
//!   reason. A voice agent wants neither, so the OpenAI dialect asks for
//!   `reasoning_effort: "none"` unless it is configured otherwise.
//! - A tool call arrives in pieces, and several calls one after the
//!   other. Each is handed on whole, as soon as the next one begins or
//!   the model stops, so that the first tool can start while the model
//!   still writes.
//! - What the response cost comes in a chunk of its own, after the model
//!   has said why it stopped.
//!
//! The adapter starts no task. The stream it returns is the loop that
//! reads the answer: dropping it closes the connection, which is how a
//! response is cancelled.

mod wire;

use core::fmt;
use core::pin::Pin;

use waveport_adapter_kit::http::{HttpClient, HttpRequest, HttpSettings};
use waveport_adapter_kit::secret::Secret;
use waveport_adapter_kit::sse::SseParser;
use waveport_adapter_kit::stream;
use waveport_ports::error::{BoxFuture, PortError};
use waveport_ports::llm::{Llm, LlmCaps, LlmEvent, LlmRequest, LlmRx};

use crate::wire::Reader;

/// Which servers the adapter speaks to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dialect {
    /// OpenAI itself: the limit on output is `max_completion_tokens`, the
    /// cost of a response is asked for, and reasoning can be set.
    OpenAi,
    /// Any server that speaks the protocol. The limit on output is
    /// `max_tokens`, and nothing else is asked for.
    Generic,
}

/// How the adapter is set up.
#[derive(Clone, Debug)]
pub struct Config {
    /// Where the API is, without the path of the request and without a
    /// slash at the end: `https://api.openai.com/v1`.
    pub base_url: String,
    /// The model.
    pub model: String,
    /// The key, for a server that wants one.
    pub key: Option<Secret>,
    /// Which servers this is for.
    pub dialect: Dialect,
    /// How much the model reasons before it answers, in the provider's
    /// words. Only the OpenAI dialect sends it.
    pub reasoning_effort: Option<String>,
    /// Connection settings.
    pub http: HttpSettings,
}

impl Config {
    /// OpenAI, with a model that is told not to reason before it answers.
    #[must_use]
    pub fn openai(model: &str, key: Option<Secret>) -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_owned(),
            model: model.to_owned(),
            key,
            dialect: Dialect::OpenAi,
            reasoning_effort: Some("none".to_owned()),
            http: HttpSettings::default(),
        }
    }

    /// A server that speaks the protocol, at `base_url`, with no key.
    #[must_use]
    pub fn generic(base_url: &str, model: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            model: model.to_owned(),
            key: None,
            dialect: Dialect::Generic,
            reasoning_effort: None,
            http: HttpSettings::default(),
        }
    }
}

/// A language model behind an OpenAI-compatible API.
#[derive(Clone)]
pub struct OpenAiCompat {
    config: Config,
    url: String,
    http: HttpClient,
}

impl fmt::Debug for OpenAiCompat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiCompat")
            .field("url", &self.url)
            .field("model", &self.config.model)
            .field("dialect", &self.config.dialect)
            .finish_non_exhaustive()
    }
}

impl OpenAiCompat {
    /// An adapter. No connection is made until a response is asked for.
    ///
    /// # Errors
    ///
    /// TLS cannot be set up on this machine.
    pub fn new(config: Config) -> Result<Self, PortError> {
        let http = HttpClient::new(config.http)?;
        let url = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));
        Ok(Self { config, url, http })
    }
}

impl Llm for OpenAiCompat {
    fn caps(&self) -> LlmCaps {
        LlmCaps {
            tools: true,
            parallel_tools: true,
        }
    }

    fn start<'a>(
        &'a self,
        request: LlmRequest<'a>,
    ) -> BoxFuture<'a, Result<Pin<Box<dyn LlmRx>>, PortError>> {
        Box::pin(async move {
            let body = wire::body(&self.config, &request)?;
            let mut response = self
                .http
                .post(HttpRequest {
                    url: &self.url,
                    headers: &[("accept", "text/event-stream")],
                    key: self.config.key.as_ref(),
                    json: &body,
                })
                .await?;

            let rx = stream::events(move |mut events| async move {
                let mut parser = SseParser::new();
                let mut reader = Reader::default();
                let mut wrote: Vec<LlmEvent> = Vec::new();
                let mut failed = None;
                while !reader.is_over() {
                    let Some(bytes) = response.chunk().await? else {
                        break;
                    };
                    let read = parser.push(&bytes, |event| {
                        if failed.is_none()
                            && let Err(error) = reader.chunk(event.data, &mut wrote)
                        {
                            failed = Some(error);
                        }
                    });
                    // What the model wrote before something went wrong is
                    // still handed on.
                    for event in wrote.drain(..) {
                        events.send(event).await;
                    }
                    if let Some(error) = failed.take() {
                        return Err(error);
                    }
                    read.map_err(|error| {
                        PortError::transient(format!("the model's answer cannot be read: {error}"))
                    })?;
                }
                match reader.end() {
                    Some(done) => {
                        events.send(done).await;
                        Ok(())
                    }
                    None => Err(PortError::transient(
                        "the model's answer ended before the model said why it stopped",
                    )),
                }
            });
            let rx: Pin<Box<dyn LlmRx>> = Box::pin(rx);
            Ok(rx)
        })
    }
}
