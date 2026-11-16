//! OpenAI Realtime over a WebSocket: the scenarios the speech-to-speech
//! port of phase 2 rests on. Each one records all that passes to
//! `recordings/openai/<scenario>[.<tag>].jsonl`.
//!
//! ```sh
//! cargo run --release --bin openai -- <scenario> [--model <id>] [--tag <tag>]
//!     [--transcriber <id>] [--effort <minimal|low|…>] [--idle <seconds>] [--quiet]
//! ```
//!
//! The key is read from `OPENAI_API_KEY` and is never printed or written.

use std::time::Duration;

use realtime_spike::{
    Recording, Res, Run, base64, base64_len, pcm_base64, silence, speech, speech_ulaw, text_of,
};
use serde_json::{Value, json};
use waveport_adapter_kit::secret::Secret;

const URL: &str = "wss://api.openai.com/v1/realtime";
const MINI: &str = "gpt-realtime-2.1-mini";
const RATE: usize = 24_000;

struct Args {
    scenario: String,
    model: String,
    tag: Option<String>,
    transcriber: String,
    effort: Option<String>,
    idle: u64,
    quiet: bool,
}

fn args() -> Res<Args> {
    let mut it = std::env::args().skip(1);
    let scenario = it.next().ok_or("which scenario? see README.md")?;
    let mut args = Args {
        scenario,
        model: MINI.to_owned(),
        tag: None,
        transcriber: "gpt-4o-mini-transcribe".to_owned(),
        effort: None,
        idle: 60,
        quiet: false,
    };
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} takes a value"));
        match flag.as_str() {
            "--model" => args.model = value()?,
            "--tag" => args.tag = Some(value()?),
            "--transcriber" => args.transcriber = value()?,
            "--effort" => args.effort = Some(value()?),
            "--idle" => args.idle = value()?.parse().map_err(|_| "--idle takes seconds")?,
            "--quiet" => args.quiet = true,
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(args)
}

/// Replaces the audio a client message carries by its length.
fn strip_out(event: &mut Value) -> u64 {
    let mut bytes = 0;
    if text_of(event, "type") == "input_audio_buffer.append" {
        if let Some(audio) = event.get("audio").and_then(Value::as_str) {
            bytes = base64_len(audio);
            event["audio"] = json!({"bytes": bytes});
        }
    }
    hide_audio(event);
    bytes
}

/// Replaces the audio a server message carries by its length.
fn strip_in(event: &mut Value) -> u64 {
    let mut bytes = 0;
    if text_of(event, "type") == "response.output_audio.delta" {
        if let Some(delta) = event.get("delta").and_then(Value::as_str) {
            bytes = base64_len(delta);
            event["delta"] = json!({"bytes": bytes});
        }
    }
    hide_audio(event);
    bytes
}

/// Any other audio in a message (an item that was retrieved has some).
fn hide_audio(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map.iter_mut() {
                if key == "audio" {
                    if let Some(text) = inner.as_str() {
                        *inner = json!({"bytes": base64_len(text)});
                        continue;
                    }
                }
                hide_audio(inner);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(hide_audio),
        _ => {}
    }
}

struct Lab {
    run: Run,
    args: Args,
    asked: u32,
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

impl Lab {
    async fn open(args: Args, name: &str, question: &str) -> Res<Self> {
        let key = Secret::from_env("OPENAI_API_KEY").map_err(|_| "OPENAI_API_KEY is not set")?;
        let bearer = format!("Bearer {}", key.expose());
        let url = format!("{URL}?model={}", args.model);
        let file = match &args.tag {
            Some(tag) => format!("{name}.{tag}"),
            None => name.to_owned(),
        };
        let recording = Recording {
            provider: "openai",
            name: &file,
            model: &args.model,
            question,
        };
        let headers = [("authorization", bearer.as_str())];
        let mut run = Run::open(recording, &url, &headers, &[&key], strip_out, strip_in).await?;
        run.loud = !args.quiet;
        let mut lab = Self { run, args, asked: 0 };
        lab.run
            .until(secs(15), |e| text_of(e, "type") == "session.created")
            .await
            .ok_or("no session.created")?;
        Ok(lab)
    }

    /// The session every scenario starts from: the client owns turns.
    fn base(&self) -> Value {
        let mut session = json!({
            "type": "realtime",
            "output_modalities": ["audio"],
            "instructions": "You are a terse assistant in a protocol test. Answer in one short sentence.",
            "max_output_tokens": 400,
            "audio": {
                "input": {
                    "format": {"type": "audio/pcm", "rate": 24000},
                    "transcription": {"model": self.args.transcriber, "language": "en"},
                    "turn_detection": null,
                    "noise_reduction": null,
                },
                "output": {"format": {"type": "audio/pcm", "rate": 24000}, "voice": "marin"},
            },
        });
        if let Some(effort) = &self.args.effort {
            session["reasoning"] = json!({"effort": effort});
        }
        session
    }

    fn id(&mut self, what: &str) -> String {
        self.asked += 1;
        format!("{what}-{}", self.asked)
    }

    /// Sends `event` with an id of its own and waits for one of `acks`, or
    /// for an error that names the id (or names none).
    async fn ask(&mut self, what: &str, mut event: Value, acks: &[&str], wait: u64) -> Option<Value> {
        let id = self.id(what);
        event["event_id"] = json!(id);
        let at = self.run.now();
        if self.run.send(event).await.is_err() {
            return None;
        }
        let answer = self
            .run
            .until(secs(wait), |e| {
                let kind = text_of(e, "type");
                let error = &e["error"]["event_id"];
                (kind == "error" && (error == &json!(id) || error.is_null())) || acks.contains(&kind)
            })
            .await;
        let after = self.run.now() - at;
        self.run.note(json!({
            "asked": id,
            "answered_by": answer.as_ref().map(|e| text_of(e, "type").to_owned()),
            "after_ms": after,
            "error": answer.as_ref().and_then(|e| e.get("error")).cloned(),
        }));
        answer
    }

    async fn session(&mut self, what: &str, session: Value) -> Option<Value> {
        self.ask(what, json!({"type": "session.update", "session": session}), &["session.updated"], 10)
            .await
    }

    /// Sends audio in pieces of 100 ms, at the pace it is spoken or all
    /// at once, reading what comes meanwhile.
    async fn speak(&mut self, samples: &[i16], paced: bool) -> Res<()> {
        let began = tokio::time::Instant::now();
        for (index, piece) in samples.chunks(RATE / 10).enumerate() {
            self.run
                .send(json!({"type": "input_audio_buffer.append", "audio": pcm_base64(piece)}))
                .await?;
            if paced {
                let due = began + ms(100) * (u32::try_from(index).unwrap_or(u32::MAX) + 1);
                let left = due.saturating_duration_since(tokio::time::Instant::now());
                self.run.listen(left).await;
            }
        }
        Ok(())
    }

    async fn speak_ulaw(&mut self, bytes: &[u8]) -> Res<()> {
        let began = tokio::time::Instant::now();
        for (index, piece) in bytes.chunks(800).enumerate() {
            self.run
                .send(json!({"type": "input_audio_buffer.append", "audio": base64(piece)}))
                .await?;
            let due = began + ms(100) * (u32::try_from(index).unwrap_or(u32::MAX) + 1);
            let left = due.saturating_duration_since(tokio::time::Instant::now());
            self.run.listen(left).await;
        }
        Ok(())
    }

    /// Asks for a response and reads on until it is done (or refused),
    /// then says what it was and how long its parts took.
    async fn respond(&mut self, what: &str, response: Option<Value>, wait: u64) -> Option<Value> {
        let id = self.id(what);
        let mut event = json!({"type": "response.create", "event_id": id});
        if let Some(response) = response {
            event["response"] = response;
        }
        let from = self.run.seen.len();
        let at = self.run.now();
        let got_before = self.run.got;
        if self.run.send(event).await.is_err() {
            return None;
        }
        let done = self
            .run
            .until(secs(wait), |e| {
                let kind = text_of(e, "type");
                let error = &e["error"]["event_id"];
                (kind == "error" && (error == &json!(id) || error.is_null())) || kind == "response.done"
            })
            .await;
        let first = |kinds: &[&str]| {
            self.run
                .seen
                .get(from..)
                .unwrap_or_default()
                .iter()
                .find(|seen| kinds.contains(&text_of(&seen.event, "type")))
                .map(|seen| seen.t - at)
        };
        let created = first(&["response.created"]);
        let audio = first(&["response.output_audio.delta"]);
        let words = first(&["response.output_audio_transcript.delta", "response.output_text.delta"]);
        let call = first(&["response.function_call_arguments.delta", "response.function_call_arguments.done"]);
        let note = json!({
            "response_asked": id,
            "ended_by": done.as_ref().map(|e| text_of(e, "type").to_owned()),
            "status": done.as_ref().map(|e| e["response"]["status"].clone()),
            "status_details": done.as_ref().map(|e| e["response"]["status_details"].clone()),
            "said": done.as_ref().map(said),
            "error": done.as_ref().and_then(|e| e.get("error")).cloned(),
            "created_after_ms": created,
            "first_audio_after_ms": audio,
            "first_words_after_ms": words,
            "first_call_arguments_after_ms": call,
            "done_after_ms": self.run.now() - at,
            "audio_ms": (self.run.got - got_before) * 1000 / (RATE as u64),
            "usage": done.as_ref().map(|e| e["response"]["usage"].clone()),
        });
        self.run.note(note);
        done
    }

    async fn text_item(&mut self, role: &str, text: &str) -> Option<Value> {
        let kind = if role == "assistant" { "output_text" } else { "input_text" };
        let item = json!({"type": "message", "role": role, "content": [{"type": kind, "text": text}]});
        self.ask("item", json!({"type": "conversation.item.create", "item": item}), &["conversation.item.done"], 10)
            .await
    }

    async fn retrieve(&mut self, item: &str) -> Option<Value> {
        self.ask(
            "retrieve",
            json!({"type": "conversation.item.retrieve", "item_id": item}),
            &["conversation.item.retrieved"],
            10,
        )
        .await
    }

    /// The id of the last item of `kind` the provider told of.
    fn last_item(&self, role: &str) -> Option<String> {
        self.run.seen.iter().rev().find_map(|seen| {
            let kind = text_of(&seen.event, "type");
            let item = &seen.event["item"];
            let named = kind == "conversation.item.added" || kind == "conversation.item.done";
            (named && (text_of(item, "role") == role || text_of(item, "type") == role))
                .then(|| text_of(item, "id").to_owned())
        })
    }
}

/// What a response that is done said: its text, transcripts and calls.
fn said(done: &Value) -> Value {
    let mut out = Vec::new();
    for item in done["response"]["output"].as_array().into_iter().flatten() {
        match text_of(item, "type") {
            "function_call" => out.push(json!({
                "call": item["name"], "arguments": item["arguments"], "call_id": item["call_id"],
                "item": item["id"], "status": item["status"],
            })),
            _ => {
                for part in item["content"].as_array().into_iter().flatten() {
                    out.push(json!({
                        "item": item["id"], "status": item["status"], "part": part["type"],
                        "text": part.get("transcript").or_else(|| part.get("text")),
                    }));
                }
                if item["content"].as_array().is_none_or(Vec::is_empty) {
                    out.push(json!({"item": item["id"], "status": item["status"], "type": item["type"], "empty": true}));
                }
            }
        }
    }
    Value::Array(out)
}

fn padded(before_ms: usize, speech: &[i16], after_ms: usize) -> Vec<i16> {
    let mut audio = silence(RATE * before_ms / 1000);
    audio.extend_from_slice(speech);
    audio.extend(silence(RATE * after_ms / 1000));
    audio
}

// ---------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------

/// A spoken turn with the provider's own detector deciding.
async fn detector_on(args: Args) -> Res<()> {
    let kind = if args.tag.as_deref().is_some_and(|t| t.contains("semantic")) {
        json!({"type": "semantic_vad", "eagerness": "auto", "create_response": true, "interrupt_response": true})
    } else {
        json!({"type": "server_vad", "threshold": 0.5, "prefix_padding_ms": 300, "silence_duration_ms": 500,
               "create_response": true, "interrupt_response": true})
    };
    let mut lab = Lab::open(
        args,
        "detector_on",
        "A spoken turn with the provider's detector on: the order of events, the time to first audio, and whether text trails the audio it describes.",
    )
    .await?;
    let mut session = lab.base();
    session["audio"]["input"]["turn_detection"] = kind;
    lab.session("session", session).await;
    let question = speech("time", 24)?;
    lab.run.note(json!({"speech": "time", "starts_at_sample": RATE * 3 / 10, "ends_at_sample": RATE * 3 / 10 + question.len()}));
    lab.speak(&padded(300, &question, 0), true).await?;
    let spoken = lab.run.now();
    lab.run.note(json!({"last_speech_sent_at_ms": spoken}));
    // A person who has stopped speaking is silent; the line is not.
    let from = lab.run.seen.len();
    for _ in 0..40 {
        lab.speak(&silence(RATE / 10), true).await?;
        let done = lab.run.seen.get(from..).unwrap_or_default().iter().any(|s| text_of(&s.event, "type") == "response.done");
        if done {
            break;
        }
    }
    lab.run.listen(secs(3)).await;
    let first = |kind: &str| lab.run.seen.iter().find(|s| text_of(&s.event, "type") == kind).map(|s| s.t);
    let summary = json!({
        "after_last_speech_ms": {
            "speech_stopped": first("input_audio_buffer.speech_stopped").map(|t| t as i64 - spoken as i64),
            "committed": first("input_audio_buffer.committed").map(|t| t as i64 - spoken as i64),
            "response_created": first("response.created").map(|t| t as i64 - spoken as i64),
            "first_audio": first("response.output_audio.delta").map(|t| t as i64 - spoken as i64),
            "first_transcript": first("response.output_audio_transcript.delta").map(|t| t as i64 - spoken as i64),
            "input_transcription_completed": first("conversation.item.input_audio_transcription.completed").map(|t| t as i64 - spoken as i64),
            "response_done": first("response.done").map(|t| t as i64 - spoken as i64),
        }
    });
    lab.run.note(summary);
    lab.run.close().await;
    Ok(())
}

/// Turns ended by the client.
async fn client_turn(args: Args) -> Res<()> {
    let mut lab = Lab::open(
        args,
        "client_turn",
        "Turn detection off: what acknowledges a commit, when the words of the turn come against the commit and the response, and what a response asked for without a commit does.",
    )
    .await?;
    let session = lab.base();
    lab.session("session", session).await;

    // A: commit and ask for the response in the same breath.
    lab.run.note(json!({"turn": "A", "what": "audio at pace, then commit and response.create together"}));
    lab.speak(&speech("time", 24)?, true).await?;
    let committed_at = lab.run.now();
    lab.run.send(json!({"type": "input_audio_buffer.commit", "event_id": "commit-a"})).await?;
    lab.respond("respond-a", None, 40).await;
    lab.run.listen(secs(3)).await;
    let first = |lab: &Lab, kind: &str, after: u64| {
        lab.run.seen.iter().find(|s| s.t >= after && text_of(&s.event, "type") == kind).map(|s| s.t - after)
    };
    let a = json!({
        "turn": "A",
        "after_commit_ms": {
            "input_audio_buffer.committed": first(&lab, "input_audio_buffer.committed", committed_at),
            "conversation.item.added": first(&lab, "conversation.item.added", committed_at),
            "conversation.item.done": first(&lab, "conversation.item.done", committed_at),
            "response.created": first(&lab, "response.created", committed_at),
            "first response audio": first(&lab, "response.output_audio.delta", committed_at),
            "input transcription first delta": first(&lab, "conversation.item.input_audio_transcription.delta", committed_at),
            "input transcription completed": first(&lab, "conversation.item.input_audio_transcription.completed", committed_at),
            "response.done": first(&lab, "response.done", committed_at),
        }
    });
    lab.run.note(a);

    // B: a response asked for with audio in the buffer and no commit.
    lab.run.note(json!({"turn": "B", "what": "audio at pace, then response.create with no commit"}));
    lab.speak(&speech("sum", 24)?, true).await?;
    lab.respond("respond-b-uncommitted", None, 40).await;
    lab.run.listen(secs(2)).await;
    lab.ask("commit-b-after", json!({"type": "input_audio_buffer.commit"}), &["input_audio_buffer.committed"], 10).await;
    lab.respond("respond-b-committed", None, 40).await;
    lab.run.listen(secs(3)).await;

    // C: the whole turn in one message, to see what transcription alone takes.
    lab.run.note(json!({"turn": "C", "what": "the audio in one message, commit, and no response until the words have come"}));
    lab.speak(&speech("hello", 24)?, false).await?;
    let at = lab.run.now();
    lab.ask("commit-c", json!({"type": "input_audio_buffer.commit"}), &["input_audio_buffer.committed"], 10).await;
    let words = lab
        .run
        .until(secs(15), |e| text_of(e, "type") == "conversation.item.input_audio_transcription.completed")
        .await;
    lab.run.note(json!({
        "turn": "C",
        "words_after_commit_ms": lab.run.now() - at,
        "words": words.as_ref().map(|e| e["transcript"].clone()),
        "usage": words.as_ref().map(|e| e["usage"].clone()),
    }));
    lab.respond("respond-c", Some(json!({"output_modalities": ["text"]})), 30).await;
    lab.run.close().await;
    Ok(())
}

/// The provider's detector as a signal, with the client deciding.
async fn signal_only(args: Args) -> Res<()> {
    let mut lab = Lab::open(
        args,
        "signal_only",
        "The detector on, with create_response and interrupt_response false: which events still come and when, and what the provider commits by itself.",
    )
    .await?;
    let mut session = lab.base();
    session["audio"]["input"]["turn_detection"] = json!({
        "type": "server_vad", "threshold": 0.5, "prefix_padding_ms": 300, "silence_duration_ms": 500,
        "create_response": false, "interrupt_response": false,
    });
    lab.session("session", session).await;

    lab.run.note(json!({"step": 1, "what": "a spoken turn and two seconds of silence; no response may come"}));
    let question = speech("time", 24)?;
    lab.run.note(json!({"speech_starts_at_ms_of_audio": 300, "speech_ends_at_ms_of_audio": 300 + question.len() * 1000 / RATE}));
    lab.speak(&padded(300, &question, 2000), true).await?;
    lab.run.listen(secs(3)).await;
    let responses = lab.run.seen.iter().filter(|s| text_of(&s.event, "type") == "response.created").count();
    lab.run.note(json!({"step": 1, "responses_created_unasked": responses}));

    lab.run.note(json!({"step": 2, "what": "a long response asked for by the client; the user speaks again while it is still being produced"}));
    let id = lab.id("respond");
    let asked_at = lab.run.seen.len();
    let long = json!({"max_output_tokens": 2000, "instructions": "Ignore the question. Count aloud from one to sixty, one number at a time."});
    lab.run.send(json!({"type": "response.create", "event_id": id, "response": long})).await?;
    lab.run.until(secs(20), |e| text_of(e, "type") == "response.output_audio.delta").await;
    lab.speak(&padded(0, &speech("sum", 24)?, 0), true).await?;
    let spoke = lab.run.seen.get(asked_at..).unwrap_or_default().iter()
        .filter(|s| matches!(text_of(&s.event, "type"), "input_audio_buffer.speech_started" | "response.done" | "response.output_audio.done"))
        .map(|s| json!([s.t, text_of(&s.event, "type"), s.event["response"]["status"]]))
        .collect::<Vec<_>>();
    lab.run.note(json!({"step": 2, "while_the_user_spoke": spoke, "note": "a response.done here would mean the provider stopped the response by itself"}));
    lab.run.send(json!({"type": "response.cancel", "event_id": "cancel-long"})).await?;
    lab.speak(&silence(RATE * 3 / 2), true).await?;
    lab.run.listen(secs(4)).await;
    let after: Vec<String> = lab.run.seen.get(asked_at..).unwrap_or_default().iter()
        .map(|s| text_of(&s.event, "type").to_owned())
        .filter(|k| !k.contains("delta"))
        .collect();
    let status = lab.run.seen.get(asked_at..).unwrap_or_default().iter()
        .find(|s| text_of(&s.event, "type") == "response.done")
        .map(|s| s.event["response"]["status_details"].clone());
    lab.run.note(json!({"step": 2, "events_without_deltas": after, "response_status_details": status}));

    lab.run.note(json!({"step": 3, "what": "the turn the provider committed by itself is taken out again"}));
    if let Some(item) = lab.last_item("user") {
        lab.retrieve(&item).await;
        lab.ask("delete", json!({"type": "conversation.item.delete", "item_id": item}), &["conversation.item.deleted"], 10).await;
    }

    lab.run.note(json!({"step": 4, "what": "the same with the semantic detector"}));
    let mut session = json!({"type": "realtime"});
    session["audio"]["input"]["turn_detection"] = json!({
        "type": "semantic_vad", "eagerness": "auto", "create_response": false, "interrupt_response": false,
    });
    lab.session("session-semantic", session).await;
    let from = lab.run.seen.len();
    lab.speak(&padded(300, &speech("hello", 24)?, 3000), true).await?;
    lab.run.listen(secs(3)).await;
    let kinds: Vec<String> = lab.run.seen.get(from..).unwrap_or_default().iter().map(|s| text_of(&s.event, "type").to_owned()).collect();
    lab.run.note(json!({"step": 4, "events": kinds}));

    lab.run.note(json!({"step": 5, "what": "input cleared in the middle of speech the detector has seen begin"}));
    let mut session = json!({"type": "realtime"});
    session["audio"]["input"]["turn_detection"] = json!({
        "type": "server_vad", "create_response": false, "interrupt_response": false,
    });
    lab.session("session-server", session).await;
    let story = speech("story", 24)?;
    let (first, rest) = story.split_at(RATE * 5 / 2);
    let from = lab.run.seen.len();
    lab.speak(&padded(300, first, 0), true).await?;
    lab.ask("clear-mid-speech", json!({"type": "input_audio_buffer.clear"}), &["input_audio_buffer.cleared"], 10).await;
    lab.speak(&padded(0, rest, 2000), true).await?;
    lab.run.listen(secs(4)).await;
    let kinds: Vec<Value> = lab.run.seen.get(from..).unwrap_or_default().iter()
        .filter(|s| !text_of(&s.event, "type").contains("delta"))
        .map(|s| json!([s.t, text_of(&s.event, "type"), s.event.get("audio_start_ms"), s.event.get("audio_end_ms"), s.event.get("transcript")]))
        .collect();
    lab.run.note(json!({"step": 5, "events": kinds}));

    lab.run.note(json!({"step": 6, "what": "the detector as a signal with a long silence window (2 s), and the client committing by itself as soon as the speech is sent"}));
    let mut session = json!({"type": "realtime"});
    session["audio"]["input"]["turn_detection"] = json!({
        "type": "server_vad", "silence_duration_ms": 2000, "create_response": false, "interrupt_response": false,
    });
    lab.session("session-long-silence", session).await;
    let from = lab.run.seen.len();
    lab.speak(&padded(300, &speech("sum", 24)?, 0), true).await?;
    lab.ask("commit-under-the-detector", json!({"type": "input_audio_buffer.commit"}), &["input_audio_buffer.committed"], 6).await;
    lab.speak(&silence(RATE * 3), true).await?;
    lab.run.listen(secs(3)).await;
    let kinds: Vec<Value> = lab.run.seen.get(from..).unwrap_or_default().iter()
        .filter(|s| !text_of(&s.event, "type").contains("delta"))
        .map(|s| json!([s.t, text_of(&s.event, "type"), s.event.get("item_id").or_else(|| s.event["item"].get("id")), s.event.get("audio_start_ms"), s.event.get("audio_end_ms"), s.event.get("transcript"), s.event.get("error").map(|e| e["code"].clone())]))
        .collect();
    lab.run.note(json!({"step": 6, "events": kinds}));
    lab.run.close().await;
    Ok(())
}

/// Input dropped before it was committed.
async fn drop_input(args: Args) -> Res<()> {
    let mut lab = Lab::open(
        args,
        "drop_input",
        "Turn detection off: input_audio_buffer.clear in the middle of an utterance, what acknowledges it, and whether any of the dropped audio reaches the conversation.",
    )
    .await?;
    let session = lab.base();
    lab.session("session", session).await;
    let story = speech("story", 24)?;
    lab.speak(story.get(..RATE * 3).unwrap_or(&story), true).await?;
    lab.ask("clear", json!({"type": "input_audio_buffer.clear"}), &["input_audio_buffer.cleared"], 10).await;
    lab.ask("commit-empty", json!({"type": "input_audio_buffer.commit"}), &["input_audio_buffer.committed"], 10).await;
    lab.ask("clear-empty", json!({"type": "input_audio_buffer.clear"}), &["input_audio_buffer.cleared"], 10).await;
    lab.speak(&speech("sum", 24)?, true).await?;
    lab.ask("commit", json!({"type": "input_audio_buffer.commit"}), &["input_audio_buffer.committed"], 10).await;
    lab.run.until(secs(15), |e| text_of(e, "type") == "conversation.item.input_audio_transcription.completed").await;
    if let Some(item) = lab.last_item("user") {
        lab.retrieve(&item).await;
    }
    let instructions = "Repeat word for word everything the user has said in this conversation, and nothing else.";
    lab.respond("respond", Some(json!({"output_modalities": ["text"], "instructions": instructions})), 30).await;
    lab.run.close().await;
    Ok(())
}

/// A response stopped while it is spoken.
async fn interrupt(args: Args) -> Res<()> {
    let mut lab = Lab::open(
        args,
        "interrupt",
        "A response cancelled mid-audio and truncated at what was played: what still arrives, what the conversation holds afterwards, and what a second truncate or one beyond the end does.",
    )
    .await?;
    let mut session = lab.base();
    session["max_output_tokens"] = json!(1200);
    session["instructions"] = json!("You are a terse assistant in a protocol test. Do exactly what is asked.");
    lab.session("session", session).await;

    // A: cancelled while the provider is still producing.
    lab.run.note(json!({"part": "A", "what": "cancel and truncate 1500 ms after the first audio came, as if that much had been played"}));
    lab.text_item("user", "Count from one to thirty, saying each number as a word, at a calm pace.").await;
    let id = lab.id("respond");
    lab.run.send(json!({"type": "response.create", "event_id": id})).await?;
    let first = lab.run.until(secs(30), |e| text_of(e, "type") == "response.output_audio.delta").await;
    let item = first.as_ref().map(|e| text_of(e, "item_id").to_owned()).unwrap_or_default();
    let got_at_first = lab.run.got;
    lab.run.listen(ms(1500)).await;
    let got_at_cancel = lab.run.got;
    let cancel_at = lab.run.now();
    let from = lab.run.seen.len();
    lab.run.send(json!({"type": "response.cancel", "event_id": "cancel-a"})).await?;
    lab.run
        .send(json!({"type": "conversation.item.truncate", "event_id": "truncate-a", "item_id": item, "content_index": 0, "audio_end_ms": 1500}))
        .await?;
    lab.run.until(secs(20), |e| text_of(e, "type") == "response.done").await;
    lab.run.listen(secs(2)).await;
    let tail = lab.run.seen.get(from..).unwrap_or_default();
    let late: Vec<&realtime_spike::Seen> = tail.iter().filter(|s| text_of(&s.event, "type") == "response.output_audio.delta").collect();
    let note = json!({
        "part": "A",
        "item": item,
        "audio_received_before_cancel_ms": (got_at_cancel - got_at_first) * 1000 / RATE as u64,
        "played_by_then_ms": 1500,
        "audio_deltas_after_cancel": late.len(),
        "last_audio_delta_after_cancel_ms": late.last().map(|s| s.t - cancel_at),
        "audio_received_after_cancel_ms": (lab.run.got - got_at_cancel) * 1000 / RATE as u64,
        "events_after_cancel": tail.iter().filter(|s| !text_of(&s.event, "type").contains("delta"))
            .map(|s| json!([s.t - cancel_at, text_of(&s.event, "type"), s.event["response"]["status"], s.event.get("audio_end_ms"), s.event.get("error")]))
            .collect::<Vec<_>>(),
    });
    lab.run.note(note);
    lab.retrieve(&item).await;
    let ask_what_was_said = "Which numbers have you said aloud so far in this conversation? List them as digits, or write none.";
    lab.text_item("user", ask_what_was_said).await;
    lab.respond("respond-what-was-said-after-1500", Some(json!({"output_modalities": ["text"]})), 30).await;
    let truncate = |ms: u64| json!({"type": "conversation.item.truncate", "item_id": item, "content_index": 0, "audio_end_ms": ms});
    lab.ask("truncate-same-again", truncate(1500), &["conversation.item.truncated"], 10).await;
    lab.ask("truncate-earlier", truncate(1000), &["conversation.item.truncated"], 10).await;
    lab.retrieve(&item).await;
    lab.ask("truncate-later-than-before", truncate(1400), &["conversation.item.truncated"], 10).await;
    lab.ask("truncate-beyond-the-end", truncate(600_000), &["conversation.item.truncated"], 10).await;
    lab.ask("truncate-at-zero", truncate(0), &["conversation.item.truncated"], 10).await;
    lab.retrieve(&item).await;
    lab.text_item("user", ask_what_was_said).await;
    lab.respond("respond-what-was-said-after-zero", Some(json!({"output_modalities": ["text"]})), 30).await;

    // D: the same cut, sent only once the provider has said the response is over.
    lab.run.note(json!({"part": "D", "what": "cancel 1500 ms after the first audio, wait for response.done, and only then truncate at 1500 ms"}));
    lab.text_item("user", "Count from forty to seventy, saying each number as a word, at a calm pace.").await;
    lab.run.send(json!({"type": "response.create", "event_id": "respond-d"})).await?;
    let first = lab.run.until(secs(30), |e| text_of(e, "type") == "response.output_audio.delta").await;
    let item_d = first.as_ref().map(|e| text_of(e, "item_id").to_owned()).unwrap_or_default();
    lab.run.listen(ms(1500)).await;
    lab.run.send(json!({"type": "response.cancel", "event_id": "cancel-d"})).await?;
    lab.run.until(secs(20), |e| text_of(e, "type") == "response.done").await;
    lab.retrieve(&item_d).await;
    lab.ask("truncate-after-done", json!({"type": "conversation.item.truncate", "item_id": item_d, "content_index": 0, "audio_end_ms": 1500}), &["conversation.item.truncated"], 10).await;
    lab.retrieve(&item_d).await;
    lab.text_item("user", "Which numbers from forty upwards have you said aloud so far? List them as digits, or write none.").await;
    lab.respond("respond-what-was-said-d", Some(json!({"output_modalities": ["text"]})), 30).await;

    // E: cut while the response is still being produced, and again once it is over.
    lab.run.note(json!({"part": "E", "what": "cancel and truncate at 1500 ms together, as in A; then the same truncate again after response.done"}));
    lab.text_item("user", "Count from seventy to one hundred, saying each number as a word, at a calm pace.").await;
    lab.run.send(json!({"type": "response.create", "event_id": "respond-e"})).await?;
    let first = lab.run.until(secs(30), |e| text_of(e, "type") == "response.output_audio.delta").await;
    let item_e = first.as_ref().map(|e| text_of(e, "item_id").to_owned()).unwrap_or_default();
    lab.run.listen(ms(1500)).await;
    lab.run.send(json!({"type": "response.cancel", "event_id": "cancel-e"})).await?;
    lab.run
        .send(json!({"type": "conversation.item.truncate", "event_id": "truncate-e-early", "item_id": item_e, "content_index": 0, "audio_end_ms": 1500}))
        .await?;
    lab.run.until(secs(20), |e| text_of(e, "type") == "response.done").await;
    lab.run.until(secs(5), |e| text_of(e, "type") == "conversation.item.truncated").await;
    lab.retrieve(&item_e).await;
    lab.ask("truncate-e-again-after-done", json!({"type": "conversation.item.truncate", "item_id": item_e, "content_index": 0, "audio_end_ms": 1500}), &["conversation.item.truncated"], 10).await;
    lab.retrieve(&item_e).await;

    // B: a response that was done before it was cut.
    lab.run.note(json!({"part": "B", "what": "a response left to finish, then truncated at 1000 ms; then cancelled with nothing to cancel"}));
    lab.text_item("user", "Say exactly: the quick brown fox jumps over the lazy dog, then the dog sleeps.").await;
    let done = lab.respond("respond-b", None, 40).await;
    let item_b = done.as_ref().and_then(|e| e["response"]["output"][0]["id"].as_str()).unwrap_or_default().to_owned();
    lab.ask("truncate-done-item", json!({"type": "conversation.item.truncate", "item_id": item_b, "content_index": 0, "audio_end_ms": 1000}), &["conversation.item.truncated"], 10).await;
    lab.retrieve(&item_b).await;
    lab.ask("cancel-nothing", json!({"type": "response.cancel"}), &["response.done"], 5).await;
    if let Some(user) = lab.last_item("user") {
        lab.ask("truncate-user-item", json!({"type": "conversation.item.truncate", "item_id": user, "content_index": 0, "audio_end_ms": 100}), &["conversation.item.truncated"], 10).await;
    }

    // C: cancelled before it said anything.
    lab.run.note(json!({"part": "C", "what": "cancel sent right behind response.create"}));
    lab.text_item("user", "Say exactly: one two three.").await;
    let from = lab.run.seen.len();
    lab.run.send(json!({"type": "response.create", "event_id": "respond-c"})).await?;
    lab.run.send(json!({"type": "response.cancel", "event_id": "cancel-c"})).await?;
    let done = lab.run.until(secs(20), |e| matches!(text_of(e, "type"), "response.done" | "error")).await;
    lab.run.listen(secs(2)).await;
    let kinds: Vec<Value> = lab.run.seen.get(from..).unwrap_or_default().iter()
        .filter(|s| !text_of(&s.event, "type").contains("delta"))
        .map(|s| json!([text_of(&s.event, "type"), s.event["response"]["status"], s.event.get("error")]))
        .collect();
    lab.run.note(json!({"part": "C", "events": kinds, "said": done.as_ref().map(said)}));
    if let Some(item) = done.as_ref().and_then(|e| e["response"]["output"][0]["id"].as_str()) {
        let item = item.to_owned();
        lab.retrieve(&item).await;
    }
    lab.run.close().await;
    Ok(())
}

/// Responses nobody asked for, and responses that do not come.
async fn unasked(args: Args) -> Res<()> {
    let mut lab = Lab::open(
        args,
        "unasked",
        "With turn detection off: can anything make the provider speak by itself, and what does a response.create with nothing to say return.",
    )
    .await?;
    let session = lab.base();
    lab.session("session", session).await;

    lab.run.note(json!({"step": "a", "what": "audio in the buffer, no commit, eight seconds of waiting"}));
    let from = lab.run.seen.len();
    lab.speak(&speech("hello", 24)?, true).await?;
    lab.run.listen(secs(8)).await;
    lab.run.note(json!({"step": "a", "events_in_eight_seconds": lab.run.seen.len() - from}));
    lab.ask("clear", json!({"type": "input_audio_buffer.clear"}), &["input_audio_buffer.cleared"], 10).await;

    lab.run.note(json!({"step": "b", "what": "response.create on a conversation with nothing in it"}));
    lab.respond("respond-empty-conversation", None, 30).await;

    lab.run.note(json!({"step": "c", "what": "response.create again, right after the assistant spoke"}));
    lab.respond("respond-after-own-turn", None, 30).await;

    lab.run.note(json!({"step": "d", "what": "two response.create in one breath"}));
    lab.text_item("user", "Say exactly: first.").await;
    lab.run.send(json!({"type": "response.create", "event_id": "twice-1", "response": {"output_modalities": ["text"]}})).await?;
    lab.run.send(json!({"type": "response.create", "event_id": "twice-2", "response": {"output_modalities": ["text"]}})).await?;
    let from = lab.run.seen.len();
    lab.run.until(secs(20), |e| text_of(e, "type") == "response.done").await;
    lab.run.listen(secs(4)).await;
    let kinds: Vec<Value> = lab.run.seen.get(from..).unwrap_or_default().iter()
        .filter(|s| matches!(text_of(&s.event, "type"), "response.created" | "response.done" | "error"))
        .map(|s| json!([text_of(&s.event, "type"), s.event["response"]["id"], s.event.get("error")]))
        .collect();
    lab.run.note(json!({"step": "d", "events": kinds}));

    lab.run.note(json!({"step": "e", "what": "a response told to say nothing"}));
    lab.respond(
        "respond-told-to-be-silent",
        Some(json!({"conversation": "none", "input": [], "instructions": "Produce no output at all. Do not write or say a single word."})),
        30,
    )
    .await;
    lab.respond(
        "respond-told-to-be-silent-text",
        Some(json!({"conversation": "none", "input": [], "output_modalities": ["text"], "instructions": "Produce no output at all. Do not write a single word."})),
        30,
    )
    .await;

    lab.run.note(json!({"step": "f", "what": "a response that may write one token"}));
    lab.text_item("user", "Say exactly: this sentence is longer than one token.").await;
    lab.respond("respond-one-token", Some(json!({"max_output_tokens": 1})), 30).await;
    lab.respond("respond-sixteen-tokens", Some(json!({"max_output_tokens": 16})), 30).await;

    lab.run.note(json!({"step": "g", "what": "the detector on with an idle timeout of 5 s: the provider speaks by itself"}));
    let mut session = json!({"type": "realtime"});
    session["audio"]["input"]["turn_detection"] = json!({"type": "server_vad", "idle_timeout_ms": 5000, "create_response": true, "interrupt_response": true});
    lab.session("session-idle-timeout", session).await;
    lab.text_item("user", "Say exactly: are you there.").await;
    lab.respond("respond-before-idle", None, 30).await;
    let from = lab.run.seen.len();
    let waited_from = lab.run.now();
    lab.run.note(json!({"step": "g", "what": "eight seconds with nothing sent at all"}));
    lab.run.listen(secs(8)).await;
    lab.run.note(json!({"step": "g", "events_with_nothing_sent": lab.run.seen.len() - from}));
    lab.run.note(json!({"step": "g", "what": "then twelve seconds of silence sent at pace"}));
    let from = lab.run.seen.len();
    for _ in 0..120 {
        lab.speak(&silence(RATE / 10), true).await?;
        if lab.run.seen.get(from..).unwrap_or_default().iter().any(|s| text_of(&s.event, "type") == "response.done") {
            break;
        }
    }
    let done = lab.run.seen.get(from..).unwrap_or_default().iter().find(|s| text_of(&s.event, "type") == "response.done").map(|s| s.event.clone());
    let kinds: Vec<Value> = lab.run.seen.get(from..).unwrap_or_default().iter()
        .filter(|s| !text_of(&s.event, "type").contains("delta"))
        .map(|s| json!([s.t - waited_from, text_of(&s.event, "type"), s.event.get("audio_start_ms"), s.event.get("audio_end_ms")]))
        .collect();
    lab.run.note(json!({"step": "g", "events": kinds, "said": done.as_ref().map(said)}));
    let mut session = json!({"type": "realtime"});
    session["audio"]["input"]["turn_detection"] = Value::Null;
    lab.session("session-detector-off", session).await;
    lab.run.close().await;
    Ok(())
}

/// A function call.
async fn tool(args: Args) -> Res<()> {
    let mut lab = Lab::open(
        args,
        "tool",
        "A function call: the order of events, arguments as they stream, the result sent, sent late after the user has spoken again, and sent twice.",
    )
    .await?;
    let mut session = lab.base();
    session["instructions"] = json!("You are a terse assistant in a protocol test. To tell the time you must call get_time. Answer in one short sentence.");
    session["tools"] = json!([{
        "type": "function",
        "name": "get_time",
        "description": "The time of day right now in a time zone.",
        "parameters": {"type": "object", "properties": {"timezone": {"type": "string", "description": "An IANA time zone, such as Europe/Madrid. Use UTC if the user named none."}}, "required": ["timezone"]},
    }]);
    session["tool_choice"] = json!("auto");
    lab.session("session", session).await;

    lab.run.note(json!({"turn": 1, "what": "the user asks the time by voice; the result is sent as soon as the call is done"}));
    lab.speak(&speech("time", 24)?, true).await?;
    lab.ask("commit", json!({"type": "input_audio_buffer.commit"}), &["input_audio_buffer.committed"], 10).await;
    let from = lab.run.seen.len();
    let done = lab.respond("respond-call", None, 40).await;
    let kinds: Vec<Value> = lab.run.seen.get(from..).unwrap_or_default().iter()
        .map(|s| json!([s.t, text_of(&s.event, "type"), s.event["item"]["type"], s.event.get("delta").filter(|d| d.is_string())]))
        .collect();
    lab.run.note(json!({"turn": 1, "events": kinds}));
    let call = done.as_ref().map(said).and_then(|s| s.as_array().and_then(|a| a.iter().find(|x| x.get("call_id").is_some()).cloned()));
    let call_id = call.as_ref().and_then(|c| c["call_id"].as_str()).unwrap_or("call_missing").to_owned();
    let output = |call: &str, text: &str| json!({"type": "conversation.item.create", "item": {"type": "function_call_output", "call_id": call, "output": text}});
    lab.ask("result", output(&call_id, "{\"time\":\"14:07\"}"), &["conversation.item.done"], 10).await;
    lab.respond("respond-with-result", None, 40).await;

    lab.run.note(json!({"turn": 1, "what": "the same result sent a second time, and a result for a call that never was"}));
    let twice = lab.ask("result-twice", output(&call_id, "{\"time\":\"09:30\"}"), &["conversation.item.done"], 10).await;
    lab.ask("result-unknown-call", output("call_that_never_was", "{\"time\":\"00:00\"}"), &["conversation.item.done"], 10).await;
    lab.text_item("user", "What time did the tool report? If it reported more than one time, say every one.").await;
    lab.respond("respond-after-duplicate", Some(json!({"output_modalities": ["text"], "tool_choice": "none"})), 30).await;
    if let Some(item) = twice.as_ref().and_then(|e| e["item"]["id"].as_str()) {
        let item = item.to_owned();
        lab.ask("delete-duplicate", json!({"type": "conversation.item.delete", "item_id": item}), &["conversation.item.deleted"], 10).await;
    }

    lab.run.note(json!({"turn": 2, "what": "a call left unanswered while the user speaks again; its result is sent late"}));
    lab.text_item("user", "What time is it in Tokyo right now?").await;
    let done = lab.respond("respond-call-2", None, 40).await;
    let call = done.as_ref().map(said).and_then(|s| s.as_array().and_then(|a| a.iter().find(|x| x.get("call_id").is_some()).cloned()));
    let call_2 = call.as_ref().and_then(|c| c["call_id"].as_str()).unwrap_or("call_missing").to_owned();
    lab.speak(&speech("sum", 24)?, true).await?;
    lab.ask("commit-2", json!({"type": "input_audio_buffer.commit"}), &["input_audio_buffer.committed"], 10).await;
    lab.respond("respond-while-call-is-open", None, 40).await;
    lab.ask("result-late", output(&call_2, "{\"time\":\"23:07\"}"), &["conversation.item.done"], 10).await;
    lab.respond("respond-with-late-result", None, 40).await;
    lab.run.listen(secs(2)).await;
    lab.run.close().await;
    Ok(())
}

/// Items put into the conversation, and responses with their own settings.
async fn inject(args: Args) -> Res<()> {
    let mut lab = Lab::open(
        args,
        "inject",
        "Items injected (user text, assistant text, a system message, with ids of the client's and in the middle), and responses with their own instructions, out of band, and text only.",
    )
    .await?;
    let session = lab.base();
    lab.session("session", session).await;
    let item = |id: &str, role: &str, kind: &str, text: &str| {
        json!({"type": "conversation.item.create", "item": {"id": id, "type": "message", "role": role, "content": [{"type": kind, "text": text}]}})
    };
    let done = &["conversation.item.done"];
    lab.ask("user-text", item("user_one", "user", "input_text", "My favourite colour is teal."), done, 10).await;
    lab.ask("assistant-text", item("assistant_one", "assistant", "output_text", "Teal, noted."), done, 10).await;
    lab.ask("system-text", item("system_one", "system", "input_text", "The user is on a phone line. Keep answers under ten words."), done, 10).await;
    let mut between = item("user_between", "user", "input_text", "My favourite number is 41.");
    between["previous_item_id"] = json!("user_one");
    lab.ask("user-text-in-the-middle", between, done, 10).await;
    let mut first = item("system_first", "system", "input_text", "This conversation is a test.");
    first["previous_item_id"] = json!("root");
    lab.ask("system-text-at-the-root", first, done, 10).await;
    let mut nowhere = item("user_nowhere", "user", "input_text", "This goes after an item that is not there.");
    nowhere["previous_item_id"] = json!("item_that_is_not_there");
    lab.ask("after-an-unknown-item", nowhere, done, 10).await;
    lab.ask("same-id-again", item("user_one", "user", "input_text", "A second item with the first one's id."), done, 10).await;
    lab.ask("developer-text", item("developer_one", "developer", "input_text", "A developer message."), done, 10).await;
    lab.ask(
        "assistant-audio",
        json!({"type": "conversation.item.create", "item": {"id": "assistant_audio", "type": "message", "role": "assistant",
            "content": [{"type": "output_audio", "audio": pcm_base64(&speech("hello", 24)?), "transcript": "Hello there."}]}}),
        done,
        10,
    )
    .await;
    lab.ask(
        "assistant-audio-transcript-only",
        json!({"type": "conversation.item.create", "item": {"id": "assistant_said", "type": "message", "role": "assistant",
            "content": [{"type": "output_audio", "transcript": "Hello there."}]}}),
        done,
        10,
    )
    .await;
    lab.ask(
        "user-audio",
        json!({"type": "conversation.item.create", "item": {"id": "user_audio", "type": "message", "role": "user",
            "content": [{"type": "input_audio", "audio": pcm_base64(&speech("hello", 24)?)}]}}),
        done,
        10,
    )
    .await;
    lab.run.listen(secs(3)).await;

    lab.run.note(json!({"what": "responses with their own settings"}));
    let text = json!(["text"]);
    lab.respond("in-conversation-text", Some(json!({"output_modalities": text, "instructions": "List the user's favourite colour and number, then every system or developer message you can see, word for word."})), 30).await;
    let aside = lab
        .respond(
            "out-of-band",
            Some(json!({"conversation": "none", "output_modalities": text, "metadata": {"purpose": "aside"}, "instructions": "Reply with exactly the word: aside."})),
            30,
        )
        .await;
    if let Some(item) = aside.as_ref().and_then(|e| e["response"]["output"][0]["id"].as_str()) {
        let item = item.to_owned();
        lab.retrieve(&item).await;
    }
    lab.respond(
        "own-input",
        Some(json!({"conversation": "none", "output_modalities": text, "metadata": {"purpose": "own input"},
            "input": [{"type": "item_reference", "id": "user_between"}, {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Repeat the number I just told you, digits only."}]}]})),
        30,
    )
    .await;
    lab.respond("no-input", Some(json!({"conversation": "none", "output_modalities": text, "input": [], "instructions": "Say what the user's favourite colour is, or the word unknown."})), 30).await;
    let spoken = lab.respond("audio-with-own-voice-and-format", Some(json!({"instructions": "Say exactly: override.", "audio": {"output": {"voice": "cedar", "format": {"type": "audio/pcmu"}}}})), 30).await;
    lab.run.note(json!({"audio_with_own_format": spoken.as_ref().map(|e| e["response"]["audio"].clone())}));
    lab.respond("two-at-once-a", Some(json!({"conversation": "none", "output_modalities": text, "metadata": {"n": "a"}, "input": [], "instructions": "Reply with exactly: a."})), 30).await;
    // Two out-of-band responses in one breath.
    lab.run.send(json!({"type": "response.create", "event_id": "par-1", "response": {"conversation": "none", "output_modalities": ["text"], "metadata": {"n": "1"}, "input": [], "instructions": "Reply with exactly: one."}})).await?;
    lab.run.send(json!({"type": "response.create", "event_id": "par-2", "response": {"conversation": "none", "output_modalities": ["text"], "metadata": {"n": "2"}, "input": [], "instructions": "Reply with exactly: two."}})).await?;
    let from = lab.run.seen.len();
    lab.run.listen(secs(6)).await;
    let kinds: Vec<Value> = lab.run.seen.get(from..).unwrap_or_default().iter()
        .filter(|s| matches!(text_of(&s.event, "type"), "response.created" | "response.done" | "error"))
        .map(|s| json!([s.t, text_of(&s.event, "type"), s.event["response"]["metadata"], s.event["response"]["status"], s.event.get("error")]))
        .collect();
    lab.run.note(json!({"two_out_of_band_at_once": kinds}));
    lab.run.note(json!({"what": "instructions of one response against those of the session, and what the conversation holds at the end"}));
    lab.text_item("user", "What is my favourite colour?").await;
    lab.respond("own-instructions", Some(json!({"output_modalities": text, "instructions": "Whatever is asked, reply with exactly: override."})), 30).await;
    lab.text_item("user", "List every message of this conversation in order, as role and text, word for word, the system ones too.").await;
    lab.respond("what-the-conversation-holds", Some(json!({"output_modalities": text, "max_output_tokens": 1500})), 40).await;
    lab.run.close().await;
    Ok(())
}

/// Whether a response made out of band leaves anything in the conversation.
async fn aside(args: Args) -> Res<()> {
    let mut lab = Lab::open(
        args,
        "aside",
        "Does the output of a response with conversation none reach the conversation the model sees?",
    )
    .await?;
    let session = lab.base();
    lab.session("session", session).await;
    let text = json!(["text"]);
    lab.text_item("user", "Remember this: the secret fruit is plum.").await;
    let aside = lab
        .respond("out-of-band", Some(json!({"conversation": "none", "output_modalities": text, "instructions": "Reply with exactly: zebra-7731."})), 30)
        .await;
    lab.text_item("user", "Quote the most recent assistant message of this conversation word for word. If the assistant has not written anything yet, write: none.").await;
    lab.respond("what-the-model-sees", Some(json!({"output_modalities": text})), 30).await;
    if let Some(item) = aside.as_ref().and_then(|e| e["response"]["output"][0]["id"].as_str()) {
        let item = item.to_owned();
        lab.retrieve(&item).await;
        lab.ask("delete-the-aside", json!({"type": "conversation.item.delete", "item_id": item}), &["conversation.item.deleted"], 10).await;
        lab.retrieve(&item).await;
    }
    lab.ask("session-close", json!({"type": "session.close"}), &["session.closed"], 6).await;
    lab.run.listen(secs(3)).await;
    lab.run.close().await;
    Ok(())
}

/// Whether anything of a session outlives its connection.
async fn reconnect(args: Args) -> Res<()> {
    let model = args.model.clone();
    let transcriber = args.transcriber.clone();
    let quiet = args.quiet;
    let mut lab = Lab::open(args, "reconnect", "Does anything of a session outlive its connection? A first connection leaves an item and a conversation behind.", ).await?;
    let session = lab.base();
    lab.session("session", session).await;
    lab.ask(
        "item",
        json!({"type": "conversation.item.create", "item": {"id": "kept_item", "type": "message", "role": "user", "content": [{"type": "input_text", "text": "The secret fruit is plum."}]}}),
        &["conversation.item.done"],
        10,
    )
    .await;
    let done = lab.respond("respond", Some(json!({"output_modalities": ["text"], "instructions": "Reply with exactly: noted."})), 30).await;
    let conversation = done.as_ref().and_then(|e| e["response"]["conversation_id"].as_str()).unwrap_or("conv_unknown").to_owned();
    let created = lab.run.seen.iter().find(|s| text_of(&s.event, "type") == "session.created").map(|s| s.event["session"]["id"].clone());
    lab.run.note(json!({"session": created, "conversation": conversation}));
    lab.run.close().await;

    let args = Args { scenario: String::new(), model, tag: Some("second_connection".to_owned()), transcriber, effort: None, idle: 0, quiet };
    let mut lab = Lab::open(args, "reconnect", "A second connection asks for what the first left behind.").await?;
    let session = lab.base();
    lab.session("session", session).await;
    lab.retrieve("kept_item").await;
    lab.ask("session-with-the-old-conversation", json!({"type": "session.update", "session": {"type": "realtime", "conversation": conversation}}), &["session.updated"], 6).await;
    lab.respond("respond-into-the-old-conversation", Some(json!({"conversation": conversation, "output_modalities": ["text"], "instructions": "Say what the secret fruit is, or the word unknown."})), 20).await;
    lab.respond("respond-in-the-new-one", Some(json!({"output_modalities": ["text"], "instructions": "Say what the secret fruit is, or the word unknown."})), 20).await;
    lab.run.close().await;
    Ok(())
}

/// What a session lets be changed once it runs.
async fn update(args: Args) -> Res<()> {
    let mut lab = Lab::open(
        args,
        "update",
        "session.update in the middle of a session: what may change while connected and what is refused.",
    )
    .await?;
    let session = lab.base();
    lab.session("session", session).await;
    let tools = json!([{"type": "function", "name": "noop", "description": "Does nothing.", "parameters": {"type": "object", "properties": {}}}]);
    lab.session("instructions", json!({"type": "realtime", "instructions": "Answer in French, in one short sentence."})).await;
    lab.session("tools", json!({"type": "realtime", "tools": tools, "tool_choice": "none"})).await;
    lab.session("voice-before-any-audio", json!({"type": "realtime", "audio": {"output": {"voice": "cedar"}}})).await;
    lab.session("output-format-before-any-audio", json!({"type": "realtime", "audio": {"output": {"format": {"type": "audio/pcmu"}}}})).await;
    lab.session("output-format-back", json!({"type": "realtime", "audio": {"output": {"format": {"type": "audio/pcm", "rate": 24000}}}})).await;
    lab.session("input-format-before-any-audio", json!({"type": "realtime", "audio": {"input": {"format": {"type": "audio/pcmu"}}}})).await;
    lab.session("input-format-back", json!({"type": "realtime", "audio": {"input": {"format": {"type": "audio/pcm", "rate": 24000}}}})).await;

    lab.run.note(json!({"what": "a spoken turn and a spoken response, then the same changes again"}));
    lab.speak(&speech("hello", 24)?, true).await?;
    lab.ask("commit", json!({"type": "input_audio_buffer.commit"}), &["input_audio_buffer.committed"], 10).await;
    lab.respond("respond", None, 30).await;
    lab.session("voice-after-audio", json!({"type": "realtime", "audio": {"output": {"voice": "marin"}}})).await;
    lab.session("output-format-after-audio", json!({"type": "realtime", "audio": {"output": {"format": {"type": "audio/pcmu"}}}})).await;
    lab.session("output-format-back-after-audio", json!({"type": "realtime", "audio": {"output": {"format": {"type": "audio/pcm", "rate": 24000}}}})).await;
    lab.session("input-format-after-audio", json!({"type": "realtime", "audio": {"input": {"format": {"type": "audio/pcmu"}}}})).await;
    lab.session("input-format-back-after-audio", json!({"type": "realtime", "audio": {"input": {"format": {"type": "audio/pcm", "rate": 24000}}}})).await;
    lab.session("speed", json!({"type": "realtime", "audio": {"output": {"speed": 1.2}}})).await;
    lab.session("model", json!({"type": "realtime", "model": "gpt-realtime-2.1"})).await;
    lab.session("kind-of-session", json!({"type": "transcription"})).await;
    lab.session("turn-detection-on", json!({"type": "realtime", "audio": {"input": {"turn_detection": {"type": "semantic_vad", "create_response": false, "interrupt_response": false}}}})).await;
    lab.session("turn-detection-off", json!({"type": "realtime", "audio": {"input": {"turn_detection": null}}})).await;
    lab.session("transcription-model", json!({"type": "realtime", "audio": {"input": {"transcription": {"model": "gpt-4o-transcribe"}}}})).await;
    lab.session("transcription-off", json!({"type": "realtime", "audio": {"input": {"transcription": null}}})).await;
    lab.session("noise-reduction", json!({"type": "realtime", "audio": {"input": {"noise_reduction": {"type": "near_field"}}}})).await;
    lab.session("modalities-text", json!({"type": "realtime", "output_modalities": ["text"]})).await;
    lab.session("modalities-both", json!({"type": "realtime", "output_modalities": ["audio", "text"]})).await;
    lab.session("modalities-audio", json!({"type": "realtime", "output_modalities": ["audio"]})).await;
    lab.session("max-output-tokens", json!({"type": "realtime", "max_output_tokens": 100})).await;
    lab.session("truncation", json!({"type": "realtime", "truncation": {"type": "retention_ratio", "retention_ratio": 0.8, "token_limits": {"post_instructions": 8000}}})).await;
    lab.session("reasoning-effort", json!({"type": "realtime", "reasoning": {"effort": "minimal"}})).await;
    lab.session("clear-tools", json!({"type": "realtime", "tools": []})).await;
    lab.session("a-field-that-is-not-there", json!({"type": "realtime", "no_such_field": true})).await;
    lab.session("without-its-type", json!({"instructions": "No type given."})).await;

    lab.run.note(json!({"what": "a change while a response is being produced"}));
    lab.text_item("user", "Count from one to ten.").await;
    lab.run.send(json!({"type": "response.create", "event_id": "respond-long"})).await?;
    lab.run.until(secs(20), |e| text_of(e, "type") == "response.output_audio.delta").await;
    lab.session("instructions-mid-response", json!({"type": "realtime", "instructions": "Answer in Spanish."})).await;
    lab.session("speed-mid-response", json!({"type": "realtime", "audio": {"output": {"speed": 1.0}}})).await;
    lab.session("turn-detection-mid-response", json!({"type": "realtime", "audio": {"input": {"turn_detection": {"type": "server_vad", "create_response": false, "interrupt_response": false}}}})).await;
    lab.run.send(json!({"type": "response.cancel"})).await?;
    lab.run.listen(secs(3)).await;
    lab.run.close().await;
    Ok(())
}

/// Which audio the provider takes and gives.
async fn formats(args: Args) -> Res<()> {
    let model = args.model.clone();
    let transcriber = args.transcriber.clone();
    let quiet = args.quiet;
    let mut lab = Lab::open(
        args,
        "formats",
        "Which input and output audio formats and rates are taken (the product listens at 16 kHz and speaks at 24 kHz; carriers send G.711).",
    )
    .await?;
    let candidates = [
        ("pcm-24000", json!({"type": "audio/pcm", "rate": 24000})),
        ("pcm-16000", json!({"type": "audio/pcm", "rate": 16000})),
        ("pcm-8000", json!({"type": "audio/pcm", "rate": 8000})),
        ("pcm-48000", json!({"type": "audio/pcm", "rate": 48000})),
        ("pcm-no-rate", json!({"type": "audio/pcm"})),
        ("pcmu", json!({"type": "audio/pcmu"})),
        ("pcma", json!({"type": "audio/pcma"})),
        ("opus", json!({"type": "audio/opus"})),
        ("legacy-pcm16", json!("pcm16")),
        ("legacy-g711_ulaw", json!("g711_ulaw")),
    ];
    for (name, format) in &candidates {
        let answer = lab.session(&format!("input-{name}"), json!({"type": "realtime", "audio": {"input": {"format": format}}})).await;
        let now = answer.as_ref().map(|e| e["session"]["audio"]["input"]["format"].clone());
        lab.run.note(json!({"input_format_asked": format, "input_format_now": now}));
    }
    for (name, format) in &candidates {
        let answer = lab.session(&format!("output-{name}"), json!({"type": "realtime", "audio": {"output": {"format": format}}})).await;
        let now = answer.as_ref().map(|e| e["session"]["audio"]["output"]["format"].clone());
        lab.run.note(json!({"output_format_asked": format, "output_format_now": now}));
    }
    lab.run.close().await;

    for (tag, output, per_sample) in [("ulaw_both", json!({"type": "audio/pcmu"}), 1), ("ulaw_in_pcm_out", json!({"type": "audio/pcm", "rate": 24000}), 2)] {
        let args = Args { scenario: String::new(), model: model.clone(), tag: Some(tag.to_owned()), transcriber: transcriber.clone(), effort: None, idle: 0, quiet };
        let mut lab = Lab::open(args, "formats", "A spoken turn sent as G.711 mu-law at 8 kHz, as a carrier sends it.").await?;
        lab.run.bytes_per_sample_out = 1;
        lab.run.bytes_per_sample_in = per_sample;
        let mut session = lab.base();
        session["audio"]["input"]["format"] = json!({"type": "audio/pcmu"});
        session["audio"]["output"]["format"] = output;
        lab.session("session", session).await;
        lab.speak_ulaw(&speech_ulaw("time")?).await?;
        lab.ask("commit", json!({"type": "input_audio_buffer.commit"}), &["input_audio_buffer.committed"], 10).await;
        let got = lab.run.got;
        let done = lab.respond("respond", None, 40).await;
        lab.run.until(secs(10), |e| text_of(e, "type") == "conversation.item.input_audio_transcription.completed").await;
        lab.run.note(json!({
            "output_samples": lab.run.got - got,
            "output_token_details": done.as_ref().map(|e| e["response"]["usage"]["output_token_details"].clone()),
            "note": "audio tokens are 1 per 50 ms of assistant audio, which gives the length the samples must add up to",
        }));
        lab.run.close().await;
    }
    Ok(())
}

/// Limits, endings and errors.
async fn limits(args: Args) -> Res<()> {
    let idle = args.idle;
    let mut lab = Lab::open(
        args,
        "limits",
        "What the session says of its expiry, what an idle connection does, the errors malformed commands get, rate-limit events, and usage as reported.",
    )
    .await?;
    let created = lab.run.seen.iter().find(|s| text_of(&s.event, "type") == "session.created").map(|s| s.event.clone());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let expires = created.as_ref().and_then(|e| e["session"]["expires_at"].as_u64());
    lab.run.note(json!({"expires_at": expires, "seconds_from_now": expires.map(|at| at as i64 - now as i64)}));
    let session = lab.base();
    lab.session("session", session).await;

    lab.run.note(json!({"what": "commands that are wrong"}));
    let wait = &["none"];
    lab.ask("unknown-type", json!({"type": "no.such.command"}), wait, 4).await;
    lab.ask("no-type", json!({"hello": "there"}), wait, 4).await;
    lab.ask("commit-empty", json!({"type": "input_audio_buffer.commit"}), &["input_audio_buffer.committed"], 4).await;
    lab.ask("cancel-nothing", json!({"type": "response.cancel"}), &["response.done"], 4).await;
    lab.ask("cancel-unknown-response", json!({"type": "response.cancel", "response_id": "resp_that_is_not_there"}), &["response.done"], 4).await;
    lab.ask("retrieve-unknown", json!({"type": "conversation.item.retrieve", "item_id": "item_that_is_not_there"}), &["conversation.item.retrieved"], 4).await;
    lab.ask("delete-unknown", json!({"type": "conversation.item.delete", "item_id": "item_that_is_not_there"}), &["conversation.item.deleted"], 4).await;
    lab.ask("truncate-unknown", json!({"type": "conversation.item.truncate", "item_id": "item_that_is_not_there", "content_index": 0, "audio_end_ms": 10}), &["conversation.item.truncated"], 4).await;
    lab.ask("append-not-base64", json!({"type": "input_audio_buffer.append", "audio": "this is not base64 !!"}), wait, 4).await;
    lab.ask("append-half-a-sample", json!({"type": "input_audio_buffer.append", "audio": base64(&[1, 2, 3])}), wait, 4).await;
    lab.ask("clear-after-bad-appends", json!({"type": "input_audio_buffer.clear"}), &["input_audio_buffer.cleared"], 4).await;
    lab.ask("append-50-ms", json!({"type": "input_audio_buffer.append", "audio": pcm_base64(&silence(RATE / 20))}), wait, 2).await;
    lab.ask("commit-50-ms", json!({"type": "input_audio_buffer.commit"}), &["input_audio_buffer.committed"], 4).await;
    lab.ask("clear-again", json!({"type": "input_audio_buffer.clear"}), &["input_audio_buffer.cleared"], 4).await;
    lab.ask("item-with-a-wrong-part", json!({"type": "conversation.item.create", "item": {"type": "message", "role": "user", "content": [{"type": "output_text", "text": "x"}]}}), &["conversation.item.done"], 4).await;
    lab.ask("respond-with-a-wrong-field", json!({"type": "response.create", "response": {"output_modalities": ["smell"]}}), &["response.done"], 6).await;
    lab.ask("respond-with-both-modalities", json!({"type": "response.create", "response": {"output_modalities": ["audio", "text"], "conversation": "none", "input": [], "instructions": "Say: hi."}}), &["response.done"], 20).await;
    lab.ask("output-buffer-clear-on-a-websocket", json!({"type": "output_audio_buffer.clear"}), &["output_audio_buffer.cleared"], 4).await;
    lab.run.send_raw(Some("this is not JSON"), None).await?;
    lab.run.listen(secs(3)).await;
    if lab.run.closed.is_none() {
        lab.run.send_raw(None, Some(&[0, 1, 2, 3])).await?;
        lab.run.listen(secs(3)).await;
    }
    if lab.run.closed.is_some() {
        lab.run.note(json!({"what": "the provider ended the connection over a wrong message"}));
        lab.run.close().await;
        return Ok(());
    }

    lab.run.note(json!({"what": "usage and rate limits of one small response"}));
    lab.text_item("user", "Say exactly: usage.").await;
    let from = lab.run.seen.len();
    lab.respond("respond", None, 30).await;
    lab.run.listen(secs(1)).await;
    let limits: Vec<Value> = lab.run.seen.get(from..).unwrap_or_default().iter()
        .filter(|s| text_of(&s.event, "type") == "rate_limits.updated")
        .map(|s| s.event["rate_limits"].clone())
        .collect();
    lab.run.note(json!({"rate_limits": limits}));

    lab.run.note(json!({"what": "nothing sent", "seconds": idle}));
    let from = lab.run.seen.len();
    lab.run.listen(secs(idle)).await;
    lab.run.note(json!({"events_while_idle": lab.run.seen.len() - from, "still_open": lab.run.closed.is_none()}));
    if lab.run.closed.is_none() {
        lab.session("still-there", json!({"type": "realtime", "max_output_tokens": 200})).await;
    }
    lab.run.close().await;
    Ok(())
}

/// A new session given the conversation of an old one.
async fn seed(args: Args) -> Res<()> {
    let model = args.model.clone();
    let quiet = args.quiet;
    let transcriber = args.transcriber.clone();
    drop(args);
    for count in [20_usize, 100, 400] {
        let args = Args { scenario: String::new(), model: model.clone(), tag: Some(format!("{count}")), transcriber: transcriber.clone(), effort: None, idle: 0, quiet: true };
        let mut lab = Lab::open(
            args,
            "seed",
            "No resume exists: how long a new session takes to be given N items of an earlier conversation, and whether it then answers from them.",
        )
        .await?;
        lab.run.loud = !quiet && count == 20;
        let connected = lab.run.now();
        let session = lab.base();
        lab.session("session", session).await;
        let configured = lab.run.now();
        for index in 0..count {
            let (role, kind, text) = if index % 2 == 0 {
                ("user", "input_text", format!("Remember this: code word number {} is {}.", index / 2 + 1, word(index / 2)))
            } else {
                ("assistant", "output_text", format!("Code word {} noted.", index / 2 + 1))
            };
            lab.run
                .send(json!({"type": "conversation.item.create", "item": {"id": format!("seed_{index}"), "type": "message", "role": role, "content": [{"type": kind, "text": text}]}}))
                .await?;
        }
        let sent = lab.run.now();
        let last = format!("seed_{}", count - 1);
        lab.run
            .until(secs(60), |e| text_of(e, "type") == "conversation.item.done" && e["item"]["id"] == json!(last))
            .await;
        let acknowledged = lab.run.now();
        let errors = lab.run.seen.iter().filter(|s| text_of(&s.event, "type") == "error").count();
        lab.run.note(json!({
            "items": count,
            "connect_ms": connected,
            "session_update_ms": configured - connected,
            "sending_items_ms": sent - configured,
            "until_last_item_acknowledged_ms": acknowledged - configured,
            "errors": errors,
        }));
        // A tool call and its result, as history.
        let call = json!({"type": "conversation.item.create", "item": {"id": "seed_call", "type": "function_call", "call_id": "call_seeded_1", "name": "get_time", "arguments": "{\"timezone\":\"UTC\"}"}});
        lab.ask("seed-function-call", call, &["conversation.item.done"], 10).await;
        let result = json!({"type": "conversation.item.create", "item": {"id": "seed_result", "type": "function_call_output", "call_id": "call_seeded_1", "output": "{\"time\":\"11:11\"}"}});
        lab.ask("seed-function-result", result, &["conversation.item.done"], 10).await;
        lab.text_item("user", "What was code word number 3, and what time did the tool report? Answer in five words or fewer.").await;
        lab.respond("respond-from-history", Some(json!({"output_modalities": ["text"]})), 40).await;
        lab.text_item("user", "And code word number 1? One word.").await;
        lab.respond("respond-again", Some(json!({"output_modalities": ["text"]})), 40).await;
        lab.run.close().await;
    }
    Ok(())
}

fn word(index: usize) -> &'static str {
    const WORDS: [&str; 10] = ["lantern", "harbour", "walnut", "copper", "meadow", "falcon", "pebble", "saffron", "tundra", "violin"];
    WORDS.get(index % WORDS.len()).copied().unwrap_or("lantern")
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let outcome = match args() {
        Err(error) => Err(error),
        Ok(args) => match args.scenario.clone().as_str() {
            "detector_on" => detector_on(args).await,
            "client_turn" => client_turn(args).await,
            "signal_only" => signal_only(args).await,
            "drop_input" => drop_input(args).await,
            "interrupt" => interrupt(args).await,
            "unasked" => unasked(args).await,
            "tool" => tool(args).await,
            "inject" => inject(args).await,
            "aside" => aside(args).await,
            "reconnect" => reconnect(args).await,
            "update" => update(args).await,
            "formats" => formats(args).await,
            "limits" => limits(args).await,
            "seed" => seed(args).await,
            other => Err(format!("no scenario named {other}")),
        },
    };
    if let Err(error) = outcome {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
