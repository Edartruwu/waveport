//! Gemini Live over a WebSocket: the same questions as `openai.rs`, asked
//! of the provider whose commands are least like OpenAI's.
//!
//! NOT YET RUN. There was no key when this was written (16 November
//! 2026). Every message here was written from the documentation as read
//! that day, so a field name may be wrong; the first run will say.
//!
//! ```sh
//! cargo run --release --bin gemini -- all            # every scenario but the long one
//! cargo run --release --bin gemini -- <scenario> [--model <id>] [--tag <tag>] [--quiet]
//!     [--modalities-at-top] [--key-in-url] [--minutes <n>]
//! ```
//!
//! The key is read from `GEMINI_API_KEY` and is never printed or written.
//! Handles that resume a session are struck from the recordings too.

use std::time::Duration;

use realtime_spike::{Recording, Res, Run, base64_len, pcm_base64, silence, speech};
use serde_json::{Value, json};
use waveport_adapter_kit::secret::Secret;

const URL: &str = "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";
const MODEL: &str = "gemini-3.8-live";
/// What the provider listens at natively, and what the product listens at.
const IN_RATE: usize = 16_000;
/// What the provider always speaks at.
const OUT_RATE: u64 = 24_000;

#[derive(Clone)]
struct Args {
    scenario: String,
    model: String,
    tag: Option<String>,
    quiet: bool,
    /// The tutorial puts `responseModalities` beside `model`; the
    /// reference puts it in `generationConfig`. The reference is the
    /// default.
    modalities_at_top: bool,
    /// The documented way to give the key is `?key=` in the address. A
    /// header keeps it out of addresses, and is tried first.
    key_in_url: bool,
    /// How long the long scenario waits for the provider's notice.
    minutes: u64,
}

fn args() -> Res<Args> {
    let mut it = std::env::args().skip(1);
    let scenario = it.next().ok_or("which scenario? see README.md")?;
    let mut args = Args {
        scenario,
        model: MODEL.to_owned(),
        tag: None,
        quiet: false,
        modalities_at_top: false,
        key_in_url: false,
        minutes: 12,
    };
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} takes a value"));
        match flag.as_str() {
            "--model" => args.model = value()?,
            "--tag" => args.tag = Some(value()?),
            "--minutes" => args.minutes = value()?.parse().map_err(|_| "--minutes takes a number")?,
            "--quiet" => args.quiet = true,
            "--modalities-at-top" => args.modalities_at_top = true,
            "--key-in-url" => args.key_in_url = true,
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(args)
}

/// Replaces the audio a client message carries by its length.
fn strip_out(event: &mut Value) -> u64 {
    let Some(audio) = event.pointer_mut("/realtimeInput/audio/data") else {
        return 0;
    };
    let bytes = audio.as_str().map_or(0, base64_len);
    *audio = json!({"bytes": bytes});
    bytes
}

/// Replaces the audio a server message carries by its length.
fn strip_in(event: &mut Value) -> u64 {
    let mut bytes = 0;
    if let Some(parts) = event.pointer_mut("/serverContent/modelTurn/parts").and_then(Value::as_array_mut) {
        for part in parts {
            if let Some(data) = part.pointer_mut("/inlineData/data") {
                let length = data.as_str().map_or(0, base64_len);
                bytes += length;
                *data = json!({"bytes": length});
            }
        }
    }
    bytes
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

struct Lab {
    run: Run,
    args: Args,
}

/// What a session is set up with, beyond the model.
#[derive(Default)]
struct Setup {
    /// The client says where activity begins and ends.
    manual: bool,
    instruction: Option<&'static str>,
    tools: Option<Value>,
    /// `Some(None)` asks for handles; `Some(Some(handle))` resumes.
    resumption: Option<Option<String>>,
    compression: bool,
    initial_history: bool,
    no_interruption: bool,
}

impl Lab {
    async fn open(args: &Args, name: &str, question: &str, setup: &Setup) -> Res<Self> {
        let key = Secret::from_env("GEMINI_API_KEY").map_err(|_| "GEMINI_API_KEY is not set")?;
        let file = match &args.tag {
            Some(tag) => format!("{name}.{tag}"),
            None => name.to_owned(),
        };
        let recording = Recording {
            provider: "gemini",
            name: &file,
            model: &args.model,
            question,
        };
        let url = if args.key_in_url {
            format!("{URL}?key={}", key.expose())
        } else {
            URL.to_owned()
        };
        let headers: Vec<(&str, &str)> = if args.key_in_url {
            Vec::new()
        } else {
            vec![("x-goog-api-key", key.expose())]
        };
        // A handle that is being resumed is as good as a key for two hours.
        let handle = setup.resumption.clone().flatten().map(Secret::new);
        let mut secrets = vec![&key];
        if let Some(handle) = &handle {
            secrets.push(handle);
        }
        let mut run = Run::open(recording, &url, &headers, &secrets, strip_out, strip_in).await?;
        run.loud = !args.quiet;
        run.hide_fields = vec!["newHandle", "handle"];
        let mut lab = Self { run, args: args.clone() };
        lab.run.send(lab.setup_message(setup)).await?;
        lab.run
            .until(secs(20), |e| e.get("setupComplete").is_some())
            .await
            .ok_or_else(|| format!("no setupComplete: {:?}", lab.run.closed))?;
        Ok(lab)
    }

    fn setup_message(&self, setup: &Setup) -> Value {
        let mut body = json!({
            "model": format!("models/{}", self.args.model),
            "inputAudioTranscription": {},
            "outputAudioTranscription": {},
        });
        if self.args.modalities_at_top {
            body["responseModalities"] = json!(["AUDIO"]);
        } else {
            body["generationConfig"] = json!({"responseModalities": ["AUDIO"]});
        }
        let instruction = setup
            .instruction
            .unwrap_or("You are a terse assistant in a protocol test. Answer in one short sentence.");
        body["systemInstruction"] = json!({"parts": [{"text": instruction}]});
        let mut input = json!({});
        if setup.manual {
            input["automaticActivityDetection"] = json!({"disabled": true});
        }
        if setup.no_interruption {
            input["activityHandling"] = json!("NO_INTERRUPTION");
        }
        body["realtimeInputConfig"] = input;
        if let Some(tools) = &setup.tools {
            body["tools"] = tools.clone();
        }
        if let Some(handle) = &setup.resumption {
            body["sessionResumption"] = match handle {
                Some(handle) => json!({"handle": handle}),
                None => json!({}),
            };
        }
        if setup.compression {
            body["contextWindowCompression"] = json!({"slidingWindow": {}});
        }
        if setup.initial_history {
            body["historyConfig"] = json!({"initialHistoryInClientContent": true});
        }
        json!({"setup": body})
    }

    async fn input(&mut self, what: Value) -> Res<()> {
        self.run.send(json!({"realtimeInput": what})).await
    }

    /// Audio in pieces of 40 ms (the provider asks for 20 to 40), at the
    /// pace it is spoken or all at once.
    async fn audio(&mut self, samples: &[i16], paced: bool) -> Res<()> {
        let began = tokio::time::Instant::now();
        for (index, piece) in samples.chunks(IN_RATE / 25).enumerate() {
            self.input(json!({"audio": {"data": pcm_base64(piece), "mimeType": "audio/pcm;rate=16000"}}))
                .await?;
            if paced {
                let due = began + ms(40) * (u32::try_from(index).unwrap_or(u32::MAX) + 1);
                self.run.listen(due.saturating_duration_since(tokio::time::Instant::now())).await;
            }
        }
        Ok(())
    }

    /// A turn the client marks: start, audio, end.
    async fn turn(&mut self, name: &str, paced: bool) -> Res<u64> {
        self.input(json!({"activityStart": {}})).await?;
        self.audio(&speech(name, 16)?, paced).await?;
        self.input(json!({"activityEnd": {}})).await?;
        Ok(self.run.now())
    }

    async fn content(&mut self, turns: Value, complete: bool) -> Res<()> {
        self.run.send(json!({"clientContent": {"turns": turns, "turnComplete": complete}})).await
    }

    async fn say(&mut self, text: &str) -> Res<()> {
        self.content(json!([{"role": "user", "parts": [{"text": text}]}]), true).await
    }

    /// Reads on until the provider says its turn is complete (or the
    /// time runs out), and says what the turn was.
    async fn until_turn_complete(&mut self, what: &str, since: u64, wait: u64) -> Value {
        let from = self.run.seen.len();
        let got = self.run.got;
        let done = self
            .run
            .until(secs(wait), |e| e.pointer("/serverContent/turnComplete") == Some(&json!(true)))
            .await;
        let seen = self.run.seen.get(from..).unwrap_or_default();
        let first = |pointer: &str| seen.iter().find(|s| s.event.pointer(pointer).is_some()).map(|s| s.t.saturating_sub(since));
        let text = |pointer: &str| {
            seen.iter()
                .filter_map(|s| s.event.pointer(pointer).and_then(Value::as_str))
                .collect::<String>()
        };
        let note = json!({
            "turn": what,
            "turn_complete": done.is_some(),
            "after_ms": {
                "first_audio": first("/serverContent/modelTurn"),
                "first_output_transcription": first("/serverContent/outputTranscription"),
                "first_input_transcription": first("/serverContent/inputTranscription"),
                "first_interim_input_transcription": first("/serverContent/interimInputTranscription"),
                "generation_complete": first("/serverContent/generationComplete"),
                "interrupted": first("/serverContent/interrupted"),
                "waiting_for_input": first("/serverContent/waitingForInput"),
                "tool_call": first("/toolCall"),
                "tool_call_cancellation": first("/toolCallCancellation"),
                "voice_activity": first("/voiceActivity"),
                "turn_complete": first("/serverContent/turnComplete"),
            },
            "heard": text("/serverContent/inputTranscription/text"),
            "said": text("/serverContent/outputTranscription/text"),
            "audio_ms": (self.run.got - got) * 1000 / OUT_RATE,
            "interaction_status": done.as_ref().and_then(|e| e.pointer("/serverContent/interactionStatus")).cloned(),
            "usage": seen.iter().rev().find_map(|s| s.event.get("usageMetadata")).cloned(),
            "kinds_of_message": kinds(seen),
        });
        self.run.note(note.clone());
        note
    }

    /// The last handle the provider gave, and whether it said the session
    /// could be resumed at that point.
    fn handle(&self) -> Option<(String, bool)> {
        self.run.seen.iter().rev().find_map(|seen| {
            let update = seen.event.get("sessionResumptionUpdate")?;
            let handle = update.get("newHandle").and_then(Value::as_str)?;
            (!handle.is_empty()).then(|| (handle.to_owned(), update["resumable"] == json!(true)))
        })
    }
}

/// Which kinds of message came, in order, with runs folded.
fn kinds(seen: &[realtime_spike::Seen]) -> Vec<String> {
    let mut out: Vec<(String, u32)> = Vec::new();
    for seen in seen {
        let mut names: Vec<String> = Vec::new();
        if let Some(map) = seen.event.as_object() {
            for (key, value) in map {
                if key == "serverContent" {
                    if let Some(inner) = value.as_object() {
                        names.extend(inner.keys().map(|k| format!("serverContent.{k}")));
                    }
                } else {
                    names.push(key.clone());
                }
            }
        }
        let name = names.join("+");
        match out.last_mut() {
            Some((last, count)) if *last == name => *count += 1,
            _ => out.push((name, 1)),
        }
    }
    out.into_iter().map(|(name, count)| if count > 1 { format!("{name} x{count}") } else { name }).collect()
}

fn time_tools(behavior: Option<&str>) -> Value {
    let mut declaration = json!({
        "name": "get_time",
        "description": "The time of day right now in a time zone.",
        "parameters": {"type": "OBJECT", "properties": {"timezone": {"type": "STRING", "description": "An IANA time zone, such as Europe/Madrid. Use UTC if the user named none."}}, "required": ["timezone"]},
    });
    if let Some(behavior) = behavior {
        declaration["behavior"] = json!(behavior);
    }
    json!([{"functionDeclarations": [declaration]}])
}

const TIME_INSTRUCTION: &str = "You are a terse assistant in a protocol test. To tell the time you must call get_time. Answer in one short sentence.";

// ---------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------

/// A turn whose bounds the client marks, streamed as it is spoken.
async fn manual_turn(args: &Args) -> Res<()> {
    let setup = Setup { manual: true, ..Setup::default() };
    let mut lab = Lab::open(
        args,
        "manual_turn",
        "Automatic detection off: activityStart, audio at pace, activityEnd. Is anything acknowledged, when does the response start, when do the words of the turn come, and does text trail the audio it describes.",
        &setup,
    )
    .await?;
    let ended = lab.turn("time", true).await?;
    lab.until_turn_complete("streamed at pace", ended, 40).await;
    lab.run.listen(secs(3)).await;
    lab.run.close().await;
    Ok(())
}

/// The same turn, held back by the client and sent only once it is over.
async fn held_input(args: &Args) -> Res<()> {
    let setup = Setup { manual: true, ..Setup::default() };
    let mut lab = Lab::open(
        args,
        "held_input",
        "Input cannot be dropped once sent, so it may have to be held until the turn commits. What does holding cost: the same turn sent in one burst, against streamed at pace.",
        &setup,
    )
    .await?;
    for (name, paced) in [("time", true), ("time", false), ("story", true), ("story", false)] {
        let ended = lab.turn(name, paced).await?;
        let what = format!("{name}, {}", if paced { "streamed at pace" } else { "held and sent in one burst" });
        lab.until_turn_complete(&what, ended, 60).await;
        lab.run.listen(secs(1)).await;
    }
    lab.run.close().await;
    Ok(())
}

/// Audio that was sent and is not wanted.
async fn drop_input(args: &Args) -> Res<()> {
    let setup = Setup { manual: true, ..Setup::default() };
    let mut lab = Lab::open(
        args,
        "drop_input",
        "There is no command that clears input. What becomes of audio sent inside an activity that is never ended, and of audio sent outside any activity?",
        &setup,
    )
    .await?;
    lab.run.note(json!({"step": 1, "what": "an activity begun, three seconds of a story, and no activityEnd for six seconds"}));
    lab.input(json!({"activityStart": {}})).await?;
    let story = speech("story", 16)?;
    lab.audio(story.get(..IN_RATE * 3).unwrap_or(&story), true).await?;
    let from = lab.run.seen.len();
    lab.run.listen(secs(6)).await;
    lab.run.note(json!({"step": 1, "came_meanwhile": kinds(lab.run.seen.get(from..).unwrap_or_default())}));

    lab.run.note(json!({"step": 2, "what": "a second activityStart with no end of the first, another question, activityEnd; then the model is asked what it heard"}));
    let ended = lab.turn("sum", true).await?;
    lab.until_turn_complete("after an activity that was abandoned", ended, 40).await;
    lab.say("Repeat word for word everything I have said aloud in this conversation, and nothing else.").await?;
    let asked = lab.run.now();
    lab.until_turn_complete("what the model heard", asked, 40).await;

    lab.run.note(json!({"step": 3, "what": "audio sent outside any activity, then a marked turn"}));
    lab.audio(&speech("hello", 16)?, true).await?;
    let from = lab.run.seen.len();
    lab.run.listen(secs(4)).await;
    lab.run.note(json!({"step": 3, "came_meanwhile": kinds(lab.run.seen.get(from..).unwrap_or_default())}));
    lab.say("Did I say the words hello there aloud at any point? Answer yes or no.").await?;
    let asked = lab.run.now();
    lab.until_turn_complete("was audio outside an activity heard", asked, 40).await;
    lab.run.close().await;
    Ok(())
}

/// A response stopped while it is spoken, three ways.
async fn interrupt(args: &Args) -> Res<()> {
    for (tag, how) in [("by_activity", 0), ("by_client_content", 1), ("by_empty_client_content", 2)] {
        let args = Args { tag: Some(tag.to_owned()), ..args.clone() };
        let setup = Setup {
            manual: true,
            instruction: Some("You are a terse assistant in a protocol test. Do exactly what is asked."),
            ..Setup::default()
        };
        let mut lab = Lab::open(
            &args,
            "interrupt",
            "There is no cancel and no truncate. A long response is stopped 1500 ms after its first audio: what still arrives, and does the history keep what was sent or what could have been heard?",
            &setup,
        )
        .await?;
        lab.say("Count from one to thirty, saying each number as a word, at a calm pace.").await?;
        lab.run.until(secs(30), |e| e.pointer("/serverContent/modelTurn").is_some()).await;
        let got_at_first = lab.run.got;
        lab.run.listen(ms(1500)).await;
        let got_at_stop = lab.run.got;
        let stop_at = lab.run.now();
        let from = lab.run.seen.len();
        match how {
            0 => lab.input(json!({"activityStart": {}})).await?,
            1 => {
                // What the user could have heard, told as the model's own turn.
                lab.content(json!([{"role": "model", "parts": [{"text": "One, two, three"}]}]), false).await?;
            }
            _ => lab.content(json!([]), false).await?,
        }
        lab.run.listen(secs(3)).await;
        let tail = lab.run.seen.get(from..).unwrap_or_default();
        let late: Vec<u64> = tail.iter().filter(|s| s.event.pointer("/serverContent/modelTurn").is_some()).map(|s| s.t - stop_at).collect();
        let note = json!({
            "how": tag,
            "audio_received_before_stop_ms": (got_at_stop - got_at_first) * 1000 / OUT_RATE,
            "played_by_then_ms": 1500,
            "audio_messages_after_stop": late.len(),
            "last_audio_after_stop_ms": late.last(),
            "audio_received_after_stop_ms": (lab.run.got - got_at_stop) * 1000 / OUT_RATE,
            "came_after_stop": kinds(tail),
            "interrupted_after_ms": tail.iter().find(|s| s.event.pointer("/serverContent/interrupted").is_some()).map(|s| s.t - stop_at),
        });
        lab.run.note(note);
        if how == 0 {
            // The activity that interrupted has to end: it ends as a turn.
            lab.audio(&speech("hello", 16)?, true).await?;
            lab.input(json!({"activityEnd": {}})).await?;
            let ended = lab.run.now();
            lab.until_turn_complete("the turn that interrupted", ended, 40).await;
        }
        lab.say("Which numbers have you said aloud so far in this conversation? List them as digits, or say none.").await?;
        let asked = lab.run.now();
        lab.until_turn_complete("what the history kept", asked, 40).await;
        lab.run.close().await;
    }
    Ok(())
}

/// A turn the model answers with silence.
async fn silence_response(args: &Args) -> Res<()> {
    let setup = Setup {
        manual: true,
        instruction: Some("You are in a room where people talk to each other. Stay silent unless somebody says the word assistant. When they do, answer in one short sentence."),
        ..Setup::default()
    };
    let mut lab = Lab::open(
        args,
        "silence",
        "Proactive audio cannot be switched off on this model: what does a turn the model chooses not to answer look like on the wire, and how long until the client can know?",
        &setup,
    )
    .await?;
    let ended = lab.turn("hello", true).await?;
    lab.until_turn_complete("a turn not meant for the model", ended, 20).await;
    let ended = lab.turn("time", true).await?;
    lab.until_turn_complete("another turn not meant for the model", ended, 20).await;
    lab.say("Assistant, are you there?").await?;
    let asked = lab.run.now();
    lab.until_turn_complete("a turn meant for the model", asked, 30).await;
    lab.run.note(json!({"what": "an activity with nothing in it but silence"}));
    lab.input(json!({"activityStart": {}})).await?;
    lab.audio(&silence(IN_RATE * 2), true).await?;
    lab.input(json!({"activityEnd": {}})).await?;
    let ended = lab.run.now();
    lab.until_turn_complete("two seconds of silence as a turn", ended, 20).await;
    lab.run.note(json!({"what": "an activity with nothing in it at all"}));
    lab.input(json!({"activityStart": {}})).await?;
    lab.input(json!({"activityEnd": {}})).await?;
    let ended = lab.run.now();
    lab.until_turn_complete("an empty activity", ended, 20).await;
    lab.run.close().await;
    Ok(())
}

/// Tool calls: blocking and not, results scheduled three ways, late,
/// twice, and cancelled by the provider.
async fn tool(args: &Args) -> Res<()> {
    let response = |id: &Value, time: &str, scheduling: Option<&str>| {
        let mut body = json!({"result": {"time": time}});
        if let Some(scheduling) = scheduling {
            body["scheduling"] = json!(scheduling);
        }
        json!({"toolResponse": {"functionResponses": [{"id": id, "name": "get_time", "response": body}]}})
    };
    for (tag, behavior) in [("non_blocking", Some("NON_BLOCKING")), ("blocking", Some("BLOCKING")), ("default", None)] {
        let args = Args { tag: Some(tag.to_owned()), ..args.clone() };
        let setup = Setup { manual: true, instruction: Some(TIME_INSTRUCTION), tools: Some(time_tools(behavior)), ..Setup::default() };
        let mut lab = Lab::open(
            &args,
            "tool",
            "A function call: the order of messages, whether the model speaks before the call, the result sent with each scheduling, sent late, sent twice.",
            &setup,
        )
        .await?;
        for (round, scheduling) in [Some("INTERRUPT"), Some("WHEN_IDLE"), Some("SILENT"), None].into_iter().enumerate() {
            lab.run.note(json!({"round": round, "scheduling": scheduling, "behavior": behavior}));
            let ended = lab.turn("time", true).await?;
            let call = lab.run.until(secs(30), |e| e.get("toolCall").is_some()).await;
            lab.run.note(json!({"round": round, "tool_call_after_ms": lab.run.now() - ended, "call": call.as_ref().map(|e| e["toolCall"].clone())}));
            let id = call.as_ref().map(|e| e["toolCall"]["functionCalls"][0]["id"].clone()).unwrap_or(Value::Null);
            // What comes while the call is open, before any result.
            let from = lab.run.seen.len();
            lab.run.listen(secs(3)).await;
            lab.run.note(json!({"round": round, "while_the_call_was_open": kinds(lab.run.seen.get(from..).unwrap_or_default())}));
            let time = format!("1{round}:07");
            lab.run.send(response(&id, &time, scheduling)).await?;
            let sent = lab.run.now();
            lab.until_turn_complete(&format!("after the result, scheduling {scheduling:?}"), sent, 30).await;
            if round == 0 {
                lab.run.note(json!({"what": "the same result a second time, and a result for a call that never was"}));
                lab.run.send(response(&id, "09:30", scheduling)).await?;
                let sent = lab.run.now();
                lab.until_turn_complete("after a duplicate result", sent, 15).await;
                lab.run.send(response(&json!("call-that-never-was"), "00:00", scheduling)).await?;
                let sent = lab.run.now();
                lab.until_turn_complete("after a result for an unknown call", sent, 15).await;
                if lab.run.closed.is_some() {
                    break;
                }
            }
        }
        if lab.run.closed.is_none() {
            lab.run.note(json!({"what": "a call left open while the user speaks again, then its result"}));
            lab.say("What time is it in Tokyo right now?").await?;
            let call = lab.run.until(secs(30), |e| e.get("toolCall").is_some()).await;
            let id = call.as_ref().map(|e| e["toolCall"]["functionCalls"][0]["id"].clone()).unwrap_or(Value::Null);
            let ended = lab.turn("sum", true).await?;
            lab.until_turn_complete("the user spoke again while the call was open", ended, 30).await;
            lab.run.send(response(&id, "23:07", Some("WHEN_IDLE"))).await?;
            let sent = lab.run.now();
            lab.until_turn_complete("after the late result", sent, 30).await;

            lab.run.note(json!({"what": "a call cancelled by the provider: the user interrupts the turn that made it"}));
            lab.say("What time is it in Lima right now?").await?;
            lab.run.until(secs(30), |e| e.get("toolCall").is_some()).await;
            let ended = lab.turn("sum", true).await?;
            lab.until_turn_complete("an interruption while a call was open", ended, 30).await;
        }
        lab.run.close().await;
    }
    Ok(())
}

/// History given to a new session, and text put into a running one.
async fn seed(args: &Args) -> Res<()> {
    const WORDS: [&str; 10] = ["lantern", "harbour", "walnut", "copper", "meadow", "falcon", "pebble", "saffron", "tundra", "violin"];
    for count in [20_usize, 100, 400] {
        let args = Args { tag: Some(format!("{count}")), quiet: args.quiet || count > 20, ..args.clone() };
        let setup = Setup { manual: true, initial_history: true, ..Setup::default() };
        let mut lab = Lab::open(
            &args,
            "seed",
            "A session that could not be resumed is given the conversation as initial history: how long N turns take, and whether the model then answers from them.",
            &setup,
        )
        .await?;
        let began = lab.run.now();
        let turns: Vec<Value> = (0..count)
            .map(|index| {
                let word = WORDS.get(index / 2 % WORDS.len()).copied().unwrap_or("lantern");
                if index % 2 == 0 {
                    json!({"role": "user", "parts": [{"text": format!("Remember this: code word number {} is {word}.", index / 2 + 1)}]})
                } else {
                    json!({"role": "model", "parts": [{"text": format!("Code word {} noted.", index / 2 + 1)}]})
                }
            })
            .collect();
        // The history ends with the model's turn and asks for nothing.
        lab.content(Value::Array(turns), true).await?;
        let from = lab.run.seen.len();
        lab.run.listen(secs(3)).await;
        lab.run.note(json!({"items": count, "came_after_the_history": kinds(lab.run.seen.get(from..).unwrap_or_default())}));
        lab.say("What was code word number 3? One word.").await?;
        let asked = lab.run.now();
        let turn = lab.until_turn_complete("answered from the history", asked, 60).await;
        lab.run.note(json!({"items": count, "history_sent_at_ms": began, "first_audio_after_question_ms": turn["after_ms"]["first_audio"]}));
        lab.run.note(json!({"what": "a model turn put into the running session, and asked about"}));
        lab.content(json!([{"role": "model", "parts": [{"text": "By the way, the door code is 4417."}]}]), false).await?;
        lab.say("What door code did you tell me? Digits only.").await?;
        let asked = lab.run.now();
        lab.until_turn_complete("a model turn injected mid-session", asked, 40).await;
        lab.run.close().await;
    }
    Ok(())
}

/// What a running session lets be changed, and which audio it takes.
async fn update_and_formats(args: &Args) -> Res<()> {
    let setup = Setup { manual: true, ..Setup::default() };
    let mut lab = Lab::open(
        args,
        "update",
        "Configuration is given once: what does a second setup message do, and which input audio is taken?",
        &setup,
    )
    .await?;
    for (name, mime, khz) in [("pcm 24 kHz", "audio/pcm;rate=24000", 24), ("pcm 16 kHz", "audio/pcm;rate=16000", 16)] {
        lab.run.note(json!({"what": format!("a turn sent as {name}")}));
        lab.input(json!({"activityStart": {}})).await?;
        for piece in speech("hello", khz)?.chunks(usize::try_from(khz).unwrap_or(16) * 40) {
            lab.input(json!({"audio": {"data": pcm_base64(piece), "mimeType": mime}})).await?;
            lab.run.listen(ms(40)).await;
        }
        lab.input(json!({"activityEnd": {}})).await?;
        let ended = lab.run.now();
        lab.until_turn_complete(name, ended, 30).await;
    }
    for mime in ["audio/pcmu", "audio/x-mulaw;rate=8000", "audio/opus"] {
        if lab.run.closed.is_some() {
            break;
        }
        lab.run.note(json!({"what": format!("160 bytes sent as {mime}")}));
        let from = lab.run.seen.len();
        lab.input(json!({"audio": {"data": realtime_spike::base64(&[0xff; 160]), "mimeType": mime}})).await?;
        lab.run.listen(secs(3)).await;
        lab.run.note(json!({"mime": mime, "came": kinds(lab.run.seen.get(from..).unwrap_or_default()), "closed": lab.run.closed}));
    }
    if lab.run.closed.is_none() {
        lab.run.note(json!({"what": "a second setup message, with other instructions"}));
        let other = Setup { manual: true, instruction: Some("Answer in French."), ..Setup::default() };
        let message = lab.setup_message(&other);
        let from = lab.run.seen.len();
        lab.run.send(message).await?;
        lab.run.listen(secs(4)).await;
        lab.run.note(json!({"came": kinds(lab.run.seen.get(from..).unwrap_or_default()), "closed": lab.run.closed}));
    }
    lab.run.close().await;

    // Wrong messages, each on a connection of its own: one may end it.
    let wrong = [
        ("an unknown message", json!({"noSuchMessage": {}})),
        ("activityEnd with no start", json!({"realtimeInput": {"activityEnd": {}}})),
        ("audioStreamEnd with detection off", json!({"realtimeInput": {"audioStreamEnd": true}})),
        ("audio that is not base64", json!({"realtimeInput": {"audio": {"data": "not base64 !!", "mimeType": "audio/pcm;rate=16000"}}})),
        ("a tool response with no call", json!({"toolResponse": {"functionResponses": [{"id": "nothing", "name": "get_time", "response": {"result": "x"}}]}})),
    ];
    for (index, (what, message)) in wrong.into_iter().enumerate() {
        let args = Args { tag: Some(format!("wrong_{index}")), ..args.clone() };
        let mut lab = Lab::open(&args, "update", "What a wrong message gets: an error message, or the end of the connection, and with which code.", &Setup { manual: true, ..Setup::default() }).await?;
        lab.run.note(json!({"what": what}));
        let from = lab.run.seen.len();
        lab.run.send(message).await?;
        lab.run.listen(secs(4)).await;
        lab.run.note(json!({"what": what, "came": kinds(lab.run.seen.get(from..).unwrap_or_default()), "closed": lab.run.closed}));
        lab.run.close().await;
    }
    Ok(())
}

/// Handles, the windows in which there is none, and a resume.
async fn resume(args: &Args) -> Res<()> {
    let setup = Setup { manual: true, resumption: Some(None), compression: true, instruction: Some(TIME_INSTRUCTION), tools: Some(time_tools(None)), ..Setup::default() };
    let mut lab = Lab::open(
        args,
        "resume",
        "Session resumption: when handles come, in which windows the session says it cannot be resumed, and what a new connection with the last handle knows.",
        &setup,
    )
    .await?;
    lab.say("Remember this: the secret fruit is plum. Reply with: noted.").await?;
    let asked = lab.run.now();
    lab.until_turn_complete("a fact to remember", asked, 30).await;
    let ended = lab.turn("time", true).await?;
    let call = lab.run.until(secs(30), |e| e.get("toolCall").is_some()).await;
    lab.run.listen(secs(2)).await;
    let id = call.as_ref().map(|e| e["toolCall"]["functionCalls"][0]["id"].clone()).unwrap_or(Value::Null);
    lab.run.note(json!({"what": "the handle while a tool call is open", "resumable": lab.handle().map(|(_, resumable)| resumable)}));
    lab.run.send(json!({"toolResponse": {"functionResponses": [{"id": id, "name": "get_time", "response": {"result": {"time": "14:07"}}}]}})).await?;
    lab.until_turn_complete("with a tool call in it", ended, 30).await;
    lab.run.listen(secs(3)).await;
    let updates: Vec<Value> = lab.run.seen.iter()
        .filter_map(|s| s.event.get("sessionResumptionUpdate").map(|u| json!([s.t, u["resumable"], u.get("newHandle").and_then(Value::as_str).map(str::len), u.get("lastConsumedClientMessageIndex")])))
        .collect();
    lab.run.note(json!({"every_update_as_time_resumable_handle_length": updates}));
    let handle = lab.handle();
    lab.run.close().await;

    let Some((handle, resumable)) = handle else {
        return Err("the provider gave no handle".to_owned());
    };
    for (tag, instruction) in [("same_setup", TIME_INSTRUCTION), ("other_instructions", "You are a terse assistant. Answer in French, in one short sentence.")] {
        let args = Args { tag: Some(format!("second_connection_{tag}")), ..args.clone() };
        let setup = Setup { manual: true, resumption: Some(Some(handle.clone())), compression: true, instruction: Some(instruction), tools: Some(time_tools(None)), ..Setup::default() };
        let began = std::time::Instant::now();
        let mut lab = match Lab::open(&args, "resume", "A new connection with the handle of the last: what it knows, and whether its setup may differ (which is how a session's configuration would be changed).", &setup).await {
            Ok(lab) => lab,
            Err(error) => {
                eprintln!("resume with {tag}: {error}");
                continue;
            }
        };
        lab.run.note(json!({"resumed_with_a_handle_that_said_resumable": resumable, "until_setup_complete_ms": began.elapsed().as_millis() as u64}));
        lab.say("What is the secret fruit, and what time did the tool report? Answer in five words or fewer.").await?;
        let asked = lab.run.now();
        lab.until_turn_complete("what the resumed session knows", asked, 30).await;
        lab.run.close().await;
    }
    Ok(())
}

/// The provider's notice that the connection is about to end. Long: the
/// connection lasts about ten minutes.
async fn go_away(args: &Args) -> Res<()> {
    let setup = Setup { manual: true, resumption: Some(None), compression: true, ..Setup::default() };
    let mut lab = Lab::open(
        args,
        "go_away",
        "A connection kept until the provider says it will end it: when goAway comes, how much time it gives, how the connection ends, and whether the handle then resumes.",
        &setup,
    )
    .await?;
    lab.say("Remember this: the secret fruit is plum. Reply with: noted.").await?;
    let asked = lab.run.now();
    lab.until_turn_complete("a fact to remember", asked, 30).await;
    let mut notice = None;
    for minute in 0..args.minutes {
        if lab.run.closed.is_some() {
            break;
        }
        // A short turn each minute, so that the session is not idle.
        let ended = lab.turn("hello", true).await?;
        lab.until_turn_complete(&format!("minute {minute}"), ended, 20).await;
        let from = lab.run.seen.len();
        lab.run.listen(secs(50)).await;
        if notice.is_none() {
            notice = lab.run.seen.get(from..).unwrap_or_default().iter().find(|s| s.event.get("goAway").is_some()).map(|s| (s.t, s.event["goAway"].clone()));
            if let Some((at, notice)) = &notice {
                lab.run.note(json!({"go_away_at_ms": at, "go_away": notice}));
            }
        }
    }
    let handle = lab.handle();
    lab.run.note(json!({"ended": lab.run.closed, "go_away": notice.as_ref().map(|(at, notice)| json!([at, notice])), "handle_resumable": handle.as_ref().map(|(_, r)| *r)}));
    lab.run.close().await;
    if let Some((handle, _)) = handle {
        let args = Args { tag: Some("second_connection".to_owned()), ..args.clone() };
        let setup = Setup { manual: true, resumption: Some(Some(handle)), compression: true, ..Setup::default() };
        let mut lab = Lab::open(&args, "go_away", "The connection after the one the provider ended.", &setup).await?;
        lab.say("What is the secret fruit? One word.").await?;
        let asked = lab.run.now();
        lab.until_turn_complete("what the resumed session knows", asked, 30).await;
        lab.run.close().await;
    }
    Ok(())
}

/// The provider's own detector, for comparison, and its forced end.
async fn detector(args: &Args) -> Res<()> {
    let mut lab = Lab::open(
        args,
        "detector",
        "Automatic detection on: voiceActivity messages and their offsets, interim transcription, and audioStreamEnd as an end of turn forced by the client.",
        &Setup::default(),
    )
    .await?;
    lab.run.note(json!({"step": 1, "what": "a question and two seconds of silence, at pace"}));
    let mut audio = silence(IN_RATE * 3 / 10);
    audio.extend(speech("time", 16)?);
    let spoken_until = audio.len();
    audio.extend(silence(IN_RATE * 2));
    lab.audio(audio.get(..spoken_until).unwrap_or(&audio), true).await?;
    let ended = lab.run.now();
    lab.audio(audio.get(spoken_until..).unwrap_or_default(), true).await?;
    lab.until_turn_complete("the provider's detector ended the turn", ended, 30).await;

    lab.run.note(json!({"step": 2, "what": "the same question, and audioStreamEnd as soon as its last sample is sent"}));
    lab.audio(&speech("time", 16)?, true).await?;
    lab.input(json!({"audioStreamEnd": true})).await?;
    let ended = lab.run.now();
    lab.until_turn_complete("the client forced the end of the turn", ended, 30).await;
    lab.run.close().await;
    Ok(())
}

async fn all(args: &Args) -> Res<()> {
    type Scenario = (&'static str, fn(&Args) -> std::pin::Pin<Box<dyn Future<Output = Res<()>> + '_>>);
    let scenarios: [Scenario; 10] = [
        ("manual_turn", |a| Box::pin(manual_turn(a))),
        ("held_input", |a| Box::pin(held_input(a))),
        ("drop_input", |a| Box::pin(drop_input(a))),
        ("interrupt", |a| Box::pin(interrupt(a))),
        ("silence", |a| Box::pin(silence_response(a))),
        ("tool", |a| Box::pin(tool(a))),
        ("seed", |a| Box::pin(seed(a))),
        ("update", |a| Box::pin(update_and_formats(a))),
        ("resume", |a| Box::pin(resume(a))),
        ("detector", |a| Box::pin(detector(a))),
    ];
    let mut failed = Vec::new();
    for (name, scenario) in scenarios {
        eprintln!("== {name}");
        if let Err(error) = scenario(args).await {
            eprintln!("{name}: {error}");
            failed.push(name);
        }
    }
    if failed.is_empty() { Ok(()) } else { Err(format!("failed: {}", failed.join(", "))) }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let outcome = match args() {
        Err(error) => Err(error),
        Ok(args) => match args.scenario.as_str() {
            "all" => all(&args).await,
            "manual_turn" => manual_turn(&args).await,
            "held_input" => held_input(&args).await,
            "drop_input" => drop_input(&args).await,
            "interrupt" => interrupt(&args).await,
            "silence" => silence_response(&args).await,
            "tool" => tool(&args).await,
            "seed" => seed(&args).await,
            "update" => update_and_formats(&args).await,
            "resume" => resume(&args).await,
            "detector" => detector(&args).await,
            "go_away" => go_away(&args).await,
            other => Err(format!("no scenario named {other}")),
        },
    };
    if let Err(error) = outcome {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
