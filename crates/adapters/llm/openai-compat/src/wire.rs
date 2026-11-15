//! What is sent and what comes back, with no connection in sight.
//!
//! A request is one JSON body. The answer is a stream of server-sent
//! events, each a JSON chunk: a little text, or a little of a tool call,
//! or why the model stopped, or what the response cost. [`Reader`] takes
//! the chunks one at a time and says what the model wrote.

use serde::{Deserialize, Serialize};
use waveport_ports::error::PortError;
use waveport_ports::llm::{Finish, LlmEvent, LlmRequest, Message, Usage};

use crate::{Config, Dialect};

/// The body of a request.
#[derive(Debug, Serialize)]
struct Body<'a> {
    model: &'a str,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
    messages: Vec<Said<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Tool<'a>>,
}

#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// One message of the conversation, as the API wants it.
#[derive(Debug, Serialize)]
struct Said<'a> {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// Text, or nothing for a message that only asks for tool calls.
    content: Content<'a>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<Call<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Content<'a> {
    Text(&'a str),
    /// Written as `null`: a message that asks for tool calls has to say
    /// that it has no text.
    Nothing,
}

#[derive(Debug, Serialize)]
struct Call<'a> {
    id: String,
    r#type: &'static str,
    function: CallFunction<'a>,
}

#[derive(Debug, Serialize)]
struct CallFunction<'a> {
    name: &'a str,
    arguments: &'a str,
}

#[derive(Debug, Serialize)]
struct Tool<'a> {
    r#type: &'static str,
    function: ToolFunction<'a>,
}

#[derive(Debug, Serialize)]
struct ToolFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: serde_json::Value,
}

impl<'a> Said<'a> {
    const fn text(role: &'static str, text: &'a str) -> Self {
        Self {
            role,
            name: None,
            content: Content::Text(text),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

/// The name a call goes by on the wire. The model's own names for its
/// calls are not kept: a session can go on with another model.
fn call_name(call: u64) -> String {
    format!("call_{call}")
}

/// The body of the request for a response.
///
/// # Errors
///
/// The parameters of a tool are not JSON.
pub(crate) fn body(config: &Config, request: &LlmRequest<'_>) -> Result<Vec<u8>, PortError> {
    let mut messages = Vec::with_capacity(request.messages.len() + 1);
    if !request.instructions.is_empty() {
        messages.push(Said::text("system", request.instructions));
    }
    for message in request.messages {
        match *message {
            Message::User { speaker, text } => messages.push(Said {
                name: speaker.map(|who| format!("participant_{}", who.get())),
                ..Said::text("user", text)
            }),
            Message::Assistant { text } => messages.push(Said::text("assistant", text)),
            Message::ToolCall { call, tool, args } => {
                let asked = Call {
                    id: call_name(call.get()),
                    r#type: "function",
                    function: CallFunction {
                        name: tool,
                        arguments: args,
                    },
                };
                // The calls of one response are one message.
                match messages.last_mut() {
                    Some(last) if !last.tool_calls.is_empty() => last.tool_calls.push(asked),
                    _ => messages.push(Said {
                        role: "assistant",
                        name: None,
                        content: Content::Nothing,
                        tool_calls: vec![asked],
                        tool_call_id: None,
                    }),
                }
            }
            Message::ToolResult { call, text } => messages.push(Said {
                tool_call_id: Some(call_name(call.get())),
                ..Said::text("tool", text)
            }),
            // Nobody said it, so it is the system's.
            Message::Notice { text } => messages.push(Said::text("system", text)),
        }
    }

    let mut tools = Vec::with_capacity(request.tools.len());
    for tool in request.tools {
        let parameters = serde_json::from_str(tool.parameters).map_err(|error| {
            PortError::permanent(format!(
                "the parameters of the tool `{}` are not JSON: {error}",
                tool.name
            ))
        })?;
        tools.push(Tool {
            r#type: "function",
            function: ToolFunction {
                name: tool.name,
                description: tool.description,
                parameters,
            },
        });
    }

    let openai = config.dialect == Dialect::OpenAi;
    let body = Body {
        model: &config.model,
        stream: true,
        stream_options: openai.then_some(StreamOptions {
            include_usage: true,
        }),
        max_completion_tokens: request.max_output_tokens.filter(|_| openai),
        max_tokens: request.max_output_tokens.filter(|_| !openai),
        reasoning_effort: config.reasoning_effort.as_deref().filter(|_| openai),
        messages,
        tools,
    };
    serde_json::to_vec(&body)
        .map_err(|error| PortError::permanent(format!("the request cannot be written: {error}")))
}

/// One chunk of an answer.
#[derive(Debug, Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Cost>,
    #[serde(default)]
    error: Option<Refusal>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Option<Delta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    /// What a model says in place of an answer it will not give. It is
    /// what the caller should hear.
    #[serde(default)]
    refusal: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<CallDelta>>,
}

#[derive(Debug, Deserialize)]
struct CallDelta {
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Cost {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<PromptDetails>,
}

#[derive(Debug, Deserialize)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: u32,
}

/// An error a server reports in the middle of an answer.
#[derive(Debug, Deserialize)]
struct Refusal {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    r#type: Option<String>,
}

/// A tool call that is still being written.
#[derive(Debug)]
struct Writing {
    index: u32,
    name: String,
    args: String,
}

/// Reads the chunks of one answer.
#[derive(Debug, Default)]
pub(crate) struct Reader {
    /// The call the model is writing.
    writing: Option<Writing>,
    /// How many calls it has finished.
    calls: usize,
    finish: Option<Finish>,
    usage: Option<Usage>,
    /// Whether the server said that the answer is over.
    over: bool,
}

impl Reader {
    /// Takes the data of one event, and adds to `out` what the model
    /// wrote in it.
    ///
    /// # Errors
    ///
    /// The server reported an error, or sent something that is not a
    /// chunk. The answer is over.
    pub(crate) fn chunk(&mut self, data: &str, out: &mut Vec<LlmEvent>) -> Result<(), PortError> {
        if self.over {
            return Ok(());
        }
        if data.trim() == "[DONE]" {
            self.over = true;
            return Ok(());
        }
        let chunk: Chunk = serde_json::from_str(data).map_err(|error| {
            PortError::transient(format!("the model's answer cannot be read: {error}"))
        })?;
        if let Some(error) = chunk.error {
            let kind = error.r#type.unwrap_or_default();
            let said = error.message.unwrap_or_default();
            return Err(PortError::transient(format!(
                "the model's answer broke off: {kind} {said}"
            )));
        }
        if let Some(cost) = chunk.usage {
            self.usage = Some(Usage {
                input: cost.prompt_tokens,
                cached: cost.prompt_tokens_details.map_or(0, |d| d.cached_tokens),
                output: cost.completion_tokens,
            });
        }
        // One answer is asked for, so there is one choice.
        let Some(choice) = chunk.choices.into_iter().next() else {
            return Ok(());
        };
        if let Some(delta) = choice.delta {
            for text in [delta.content, delta.refusal].into_iter().flatten() {
                if !text.is_empty() {
                    out.push(LlmEvent::Delta { text });
                }
            }
            for call in delta.tool_calls.into_iter().flatten() {
                self.call(call, out)?;
            }
        }
        if let Some(reason) = choice.finish_reason {
            self.finished(&reason, out)?;
        }
        Ok(())
    }

    /// A little more of a tool call.
    fn call(&mut self, delta: CallDelta, out: &mut Vec<LlmEvent>) -> Result<(), PortError> {
        let function = delta.function.unwrap_or(FunctionDelta {
            name: None,
            arguments: None,
        });
        // A piece belongs to the call being written unless it says it is
        // of another, by its number or, from a server that gives no
        // numbers, by naming a tool.
        let current = self.writing.as_ref().map(|writing| writing.index);
        let index = match (delta.index, current) {
            (Some(index), _) => index,
            (None, Some(current)) if function.name.is_none() => current,
            (None, Some(current)) => current + 1,
            (None, None) => 0,
        };
        if current.is_some_and(|current| current != index) {
            // The call before it is whole.
            self.close_call(out)?;
        }
        let writing = self.writing.get_or_insert_with(|| Writing {
            index,
            name: String::new(),
            args: String::new(),
        });
        if let Some(name) = function.name {
            writing.name.push_str(&name);
        }
        if let Some(arguments) = function.arguments {
            writing.args.push_str(&arguments);
        }
        Ok(())
    }

    /// The call being written is whole.
    fn close_call(&mut self, out: &mut Vec<LlmEvent>) -> Result<(), PortError> {
        let Some(writing) = self.writing.take() else {
            return Ok(());
        };
        if writing.name.is_empty() {
            return Err(PortError::transient(
                "the model asked for a tool call without naming a tool",
            ));
        }
        // A tool that takes nothing is called with nothing.
        let args = if writing.args.trim().is_empty() {
            "{}".to_owned()
        } else {
            writing.args
        };
        self.calls += 1;
        out.push(LlmEvent::ToolCall {
            tool: writing.name,
            args,
        });
        Ok(())
    }

    /// The model stopped, and said why.
    fn finished(&mut self, reason: &str, out: &mut Vec<LlmEvent>) -> Result<(), PortError> {
        let said = match reason {
            "length" => Finish::Length,
            "content_filter" => Finish::Filtered,
            "tool_calls" | "function_call" => Finish::ToolCalls,
            _ => Finish::Stop,
        };
        if matches!(said, Finish::Length | Finish::Filtered) {
            // A call that was being written when the model was cut off
            // is not whole, and is not asked for.
            self.writing = None;
        } else {
            self.close_call(out)?;
        }
        // A response waits for tool calls exactly when it asked for
        // some, whatever word the server has for it.
        self.finish = Some(match (self.calls, said) {
            (0, Finish::ToolCalls) => Finish::Stop,
            (0, said) => said,
            (_, _) => Finish::ToolCalls,
        });
        Ok(())
    }

    /// Whether the server said that the answer is over.
    pub(crate) const fn is_over(&self) -> bool {
        self.over
    }

    /// How the answer ended, once nothing more of it comes. `None` if
    /// the model never said why it stopped: the answer broke off.
    pub(crate) fn end(&mut self) -> Option<LlmEvent> {
        let finish = self.finish.take()?;
        Some(LlmEvent::Done {
            finish,
            usage: self.usage.take(),
        })
    }
}

#[cfg(test)]
mod tests {
    use waveport_ports::error::RetryClass;
    use waveport_ports::llm::{Finish, LlmEvent, LlmRequest, Message, ToolDecl, Usage};

    use super::{Reader, body};
    use crate::Config;

    fn written(config: &Config, request: &LlmRequest<'_>) -> serde_json::Value {
        serde_json::from_slice(&body(config, request).unwrap()).unwrap()
    }

    fn read(chunks: &[&str]) -> (Vec<LlmEvent>, Option<LlmEvent>) {
        let mut reader = Reader::default();
        let mut out = Vec::new();
        for chunk in chunks {
            reader.chunk(chunk, &mut out).unwrap();
        }
        (out, reader.end())
    }

    fn delta(text: &str) -> LlmEvent {
        LlmEvent::Delta {
            text: text.to_owned(),
        }
    }

    fn call(tool: &str, args: &str) -> LlmEvent {
        LlmEvent::ToolCall {
            tool: tool.to_owned(),
            args: args.to_owned(),
        }
    }

    const fn done(finish: Finish) -> LlmEvent {
        LlmEvent::Done {
            finish,
            usage: None,
        }
    }

    #[test]
    fn a_conversation_is_written_as_the_api_wants_it() {
        use waveport_domain::ids::{ParticipantId, ToolCallId};
        let messages = [
            Message::User {
                speaker: Some(ParticipantId(2)),
                text: "What time is it in Lima and in Madrid?",
            },
            Message::Assistant {
                text: "One moment.",
            },
            Message::ToolCall {
                call: ToolCallId(7),
                tool: "get_time",
                args: "{\"city\":\"Lima\"}",
            },
            Message::ToolCall {
                call: ToolCallId(8),
                tool: "get_time",
                args: "{\"city\":\"Madrid\"}",
            },
            Message::ToolResult {
                call: ToolCallId(7),
                text: "09:15",
            },
            Message::ToolResult {
                call: ToolCallId(8),
                text: "16:15",
            },
            Message::Notice {
                text: "The caller interrupted you.",
            },
            Message::User {
                speaker: None,
                text: "And in Tokyo?",
            },
        ];
        let tools = [ToolDecl {
            name: "get_time",
            description: "The time of day in a city.",
            parameters: "{\"type\":\"object\",\"properties\":{\"city\":{\"type\":\"string\"}}}",
        }];
        let request = LlmRequest {
            instructions: "You are a helpful agent on a call.",
            messages: &messages,
            tools: &tools,
            max_output_tokens: Some(300),
        };
        let config = Config::openai("gpt-test", None);
        let expected = serde_json::json!({
            "model": "gpt-test",
            "stream": true,
            "stream_options": {"include_usage": true},
            "max_completion_tokens": 300,
            "reasoning_effort": "none",
            "messages": [
                {"role": "system", "content": "You are a helpful agent on a call."},
                {"role": "user", "name": "participant_2",
                 "content": "What time is it in Lima and in Madrid?"},
                {"role": "assistant", "content": "One moment."},
                // The two calls of one response are one message.
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_7", "type": "function",
                     "function": {"name": "get_time", "arguments": "{\"city\":\"Lima\"}"}},
                    {"id": "call_8", "type": "function",
                     "function": {"name": "get_time", "arguments": "{\"city\":\"Madrid\"}"}},
                ]},
                {"role": "tool", "tool_call_id": "call_7", "content": "09:15"},
                {"role": "tool", "tool_call_id": "call_8", "content": "16:15"},
                {"role": "system", "content": "The caller interrupted you."},
                {"role": "user", "content": "And in Tokyo?"},
            ],
            "tools": [{"type": "function", "function": {
                "name": "get_time",
                "description": "The time of day in a city.",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
            }}],
        });
        assert_eq!(written(&config, &request), expected);
    }

    #[test]
    fn a_server_that_is_only_compatible_is_asked_for_less() {
        let messages = [Message::User {
            speaker: None,
            text: "Hello.",
        }];
        let request = LlmRequest {
            instructions: "",
            messages: &messages,
            tools: &[],
            max_output_tokens: Some(64),
        };
        let config = Config::generic("http://localhost:8080/v1", "local");
        // No instructions, no tools, and nothing that only one provider
        // knows.
        let expected = serde_json::json!({
            "model": "local",
            "stream": true,
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "Hello."}],
        });
        assert_eq!(written(&config, &request), expected);

        // Without a limit, none is named.
        let request = LlmRequest {
            max_output_tokens: None,
            ..request
        };
        let body = written(&Config::openai("gpt-test", None), &request);
        assert_eq!(body.get("max_completion_tokens"), None);
        assert_eq!(body.get("max_tokens"), None);
    }

    #[test]
    fn parameters_that_are_not_json_are_refused_before_anything_is_sent() {
        let tools = [ToolDecl {
            name: "broken",
            description: "",
            parameters: "{not json",
        }];
        let request = LlmRequest {
            instructions: "",
            messages: &[],
            tools: &tools,
            max_output_tokens: None,
        };
        let error = body(&Config::openai("gpt-test", None), &request).unwrap_err();
        assert_eq!(error.class, RetryClass::Permanent);
        assert!(error.message.contains("`broken`"), "{error}");
    }

    #[test]
    fn text_is_read_piece_by_piece_and_an_empty_piece_is_nothing() {
        let (events, end) = read(&[
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"","refusal":null},"finish_reason":null}],"usage":null,"obfuscation":"x"}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"Par"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"is."},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":32,"completion_tokens":10,"total_tokens":42,"prompt_tokens_details":{"cached_tokens":8}}}"#,
            "[DONE]",
        ]);
        assert_eq!(events, [delta("Par"), delta("is.")]);
        let usage = Usage {
            input: 32,
            cached: 8,
            output: 10,
        };
        assert_eq!(
            end,
            Some(LlmEvent::Done {
                finish: Finish::Stop,
                usage: Some(usage)
            })
        );
    }

    #[test]
    fn tool_calls_are_put_together_and_given_whole() {
        let mut reader = Reader::default();
        let mut out = Vec::new();
        let chunks = [
            r#"{"choices":[{"delta":{"role":"assistant","content":null}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"get_time","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"ci"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ty\": \"Lima\"}"}}]}}]}"#,
        ];
        for chunk in chunks {
            reader.chunk(chunk, &mut out).unwrap();
        }
        // Nothing yet: the call may have more to it.
        assert_eq!(out, []);
        // The next call begins, so the first is whole, and is given at
        // once: its tool can start while the model writes on.
        reader
            .chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"get_date","arguments":""}}]}}]}"#,
                &mut out,
            )
            .unwrap();
        assert_eq!(out, [call("get_time", "{\"city\": \"Lima\"}")]);
        reader
            .chunk(
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                &mut out,
            )
            .unwrap();
        // A tool that takes nothing is called with nothing.
        assert_eq!(out[1], call("get_date", "{}"));
        assert_eq!(reader.end(), Some(done(Finish::ToolCalls)));
        // An end is given once.
        assert_eq!(reader.end(), None);
    }

    #[test]
    fn servers_that_do_it_their_own_way_are_understood() {
        // Calls that come whole, without numbers, and a server that
        // calls it a stop.
        let (events, end) = read(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"function":{"name":"a","arguments":"{\"x\":1}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"function":{"name":"b","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]);
        assert_eq!(events, [call("a", "{\"x\":1}"), call("b", "{}")]);
        assert_eq!(end, Some(done(Finish::ToolCalls)));

        // A server that says it waits for calls and asked for none.
        let (events, end) = read(&[
            r#"{"choices":[{"delta":{"content":"Hi."}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        assert_eq!(events, [delta("Hi.")]);
        assert_eq!(end, Some(done(Finish::Stop)));

        // A word for stopping that this adapter does not know.
        let (_, end) =
            read(&[r#"{"choices":[{"delta":{"content":"Hi."},"finish_reason":"eos"}]}"#]);
        assert_eq!(end, Some(done(Finish::Stop)));

        // What a model says when it will not answer is what is heard.
        let (events, _) =
            read(&[r#"{"choices":[{"delta":{"refusal":"I cannot help with that."}}]}"#]);
        assert_eq!(events, [delta("I cannot help with that.")]);
    }

    #[test]
    fn a_call_that_was_cut_off_is_not_asked_for() {
        let (events, end) = read(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"a","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"name":"b","arguments":"{\"x\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#,
        ]);
        // The first was whole, and the response waits for it.
        assert_eq!(events, [call("a", "{}")]);
        assert_eq!(end, Some(done(Finish::ToolCalls)));

        let (events, end) = read(&[
            r#"{"choices":[{"delta":{"content":"It is"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#,
        ]);
        assert_eq!(events, [delta("It is")]);
        assert_eq!(end, Some(done(Finish::Length)));

        let (_, end) = read(&[r#"{"choices":[{"delta":{},"finish_reason":"content_filter"}]}"#]);
        assert_eq!(end, Some(done(Finish::Filtered)));
    }

    #[test]
    fn an_answer_that_never_says_why_it_stopped_has_no_end() {
        let (events, end) = read(&[r#"{"choices":[{"delta":{"content":"It is"}}]}"#]);
        assert_eq!(events, [delta("It is")]);
        assert_eq!(end, None);
    }

    #[test]
    fn what_cannot_be_read_and_what_the_server_reports_end_the_answer() {
        let mut reader = Reader::default();
        let mut out = Vec::new();
        let error = reader.chunk("{not json", &mut out).unwrap_err();
        assert_eq!(error.class, RetryClass::Transient);

        let mut reader = Reader::default();
        let error = reader
            .chunk(
                r#"{"error":{"message":"The server had an error","type":"server_error"}}"#,
                &mut out,
            )
            .unwrap_err();
        assert_eq!(error.class, RetryClass::Transient);
        assert!(error.message.contains("server_error"), "{error}");

        let mut reader = Reader::default();
        let error = reader
            .chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
                &mut out,
            )
            .unwrap_err();
        assert!(error.message.contains("without naming"), "{error}");
        assert_eq!(out, []);
    }

    #[test]
    fn nothing_is_read_after_the_server_said_it_was_done() {
        let mut reader = Reader::default();
        let mut out = Vec::new();
        reader.chunk("[DONE]", &mut out).unwrap();
        assert!(reader.is_over());
        reader
            .chunk(r#"{"choices":[{"delta":{"content":"more"}}]}"#, &mut out)
            .unwrap();
        assert_eq!(out, []);
    }
}
