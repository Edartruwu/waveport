//! What stands in for the providers of a whole agent.
//!
//! A test that runs a program from its command line to what its far end
//! hears needs something in place of each provider: a language model
//! that answers what the test wants answered, a recogniser that hears
//! the question of [`speech`](crate::speech), and a synthesiser with a
//! voice that can be told from silence. The pieces here are given to
//! the [`servers`](crate::servers), which play them:
//!
//! - **The language model** answers in the shape of a stream of chat
//!   completions, as an OpenAI-compatible server sends one
//!   ([`answer`], [`slow_answer`], [`unfinished`]).
//! - **The recogniser** says what OpenAI once said when it was sent
//!   [`what_time_is_it`](crate::speech::what_time_is_it), whatever it
//!   is sent ([`HEARD`]).
//! - **The synthesiser** says [`SAID`] in the shapes Cartesia's
//!   documentation gives, with a tone for a voice ([`spoken`]), and
//!   [`GREETING`] before it for an agent that greets ([`greeted`]).
//!
//! Everything in them is made up, or was recorded of a machine's voice:
//! no key, and nobody's name or number.

use core::time::Duration;

use waveport_adapter_kit::pcm::write_base64;
use waveport_ports::audio::Rate;

use crate::servers::{End, HttpAnswer, WsStep};

/// What the recogniser said when it was sent the question of
/// [`what_time_is_it`](crate::speech::what_time_is_it): the script of a
/// server that stands in for it.
pub const HEARD: &str = include_str!("../../../adapters/stt/openai/tests/fixtures/question.ws");

/// What an agent says once it knows the time, in the pieces a language
/// model writes it in. [`spoken`] is the voice of exactly this.
pub const SAID: [&str; 3] = ["It is", " ten past three.", " Goodbye!"];

/// What an agent that greets says first. [`greeted`] is the voice of
/// exactly this.
pub const GREETING: &str = "Good morning.";

/// One piece of an answer of the language model, as it comes off the
/// wire: `delta` is what the piece adds, and `finish` why the answer
/// ends with it, both as JSON.
#[must_use]
pub fn piece(delta: &str, finish: &str) -> String {
    format!(
        "data: {{\"id\":\"chatcmpl-made-up\",\"object\":\"chat.completion.chunk\",\
         \"created\":1794769429,\"model\":\"a-model\",\"choices\":[{{\"index\":0,\
         \"delta\":{delta},\"finish_reason\":{finish}}}]}}\n\n"
    )
}

/// The pieces an answer begins with: who writes it, and these words.
fn begun(words: &[&str]) -> String {
    let mut stream = piece("{\"role\":\"assistant\",\"content\":\"\"}", "null");
    for word in words {
        stream.push_str(&piece(&format!("{{\"content\":\"{word}\"}}"), "null"));
    }
    stream
}

/// An answer of the language model as text: these words, and then a
/// call of the tool `tool` with no arguments, if one is named.
#[must_use]
pub fn answer_text(words: &[&str], tool: Option<&str>) -> String {
    let mut stream = begun(words);
    match tool {
        Some(name) => {
            let call = format!(
                "{{\"tool_calls\":[{{\"index\":0,\"id\":\"call_{name}\",\"type\":\"function\",\
                 \"function\":{{\"name\":\"{name}\",\"arguments\":\"{{}}\"}}}}]}}"
            );
            stream.push_str(&piece(&call, "null"));
            stream.push_str(&piece("{}", "\"tool_calls\""));
        }
        None => stream.push_str(&piece("{}", "\"stop\"")),
    }
    stream.push_str("data: [DONE]\n\n");
    stream
}

/// An answer of the language model that arrives at once.
#[must_use]
pub fn answer(words: &[&str], tool: Option<&str>) -> HttpAnswer {
    HttpAnswer::recorded(&answer_text(words, tool))
}

/// The same answer with no tool, with `pause` before each of its lines.
#[must_use]
pub fn slow_answer(words: &[&str], pause: Duration) -> HttpAnswer {
    HttpAnswer::Stream {
        pieces: answer_text(words, None)
            .split_inclusive('\n')
            .map(|line| line.as_bytes().to_vec())
            .collect(),
        pause,
        end: End::Complete,
    }
}

/// An answer that stops after these words and is never finished: the
/// server says nothing more, and keeps the connection.
#[must_use]
pub fn unfinished(words: &[&str]) -> HttpAnswer {
    HttpAnswer::Stream {
        pieces: begun(words)
            .split_inclusive("\n\n")
            .map(|piece| piece.as_bytes().to_vec())
            .collect(),
        pause: Duration::ZERO,
        end: End::Hang,
    }
}

/// 400 ms of a tone at 24 kHz, as the synthesiser sends a piece of
/// audio.
fn tone() -> String {
    let tone: Vec<i16> = (0..Rate::Hz24000.samples_in_millis(400))
        .map(|n| if n % 48 < 24 { 3_000 } else { -3_000 })
        .collect();
    let mut data = String::new();
    write_base64(&tone, &mut data);
    format!(
        "< {{\"type\":\"chunk\",\"data\":\"{data}\",\"done\":false,\"status_code\":206,\
         \"step_time\":12,\"context_id\":\"{{{{context_id}}}}\"}}"
    )
}

/// Where words are in the audio, as the synthesiser says it.
fn timed(words: &str, starts: &str, ends: &str) -> String {
    format!(
        "< {{\"type\":\"timestamps\",\"done\":false,\"status_code\":206,\
         \"context_id\":\"{{{{context_id}}}}\",\"word_timestamps\":{{\"words\":[{words}],\
         \"start\":[{starts}],\"end\":[{ends}]}}}}"
    )
}

/// That a response has been spoken whole, as the synthesiser says it.
const DONE: &str = "< {\"type\":\"done\",\"done\":true,\"status_code\":206,\
                    \"context_id\":\"{{context_id}}\"}";

/// What the synthesiser says for one response whose text is [`SAID`]:
/// 1.2 s of a tone at 24 kHz, in three pieces, with where the words
/// are. As the provider does, it speaks each piece of text when it is
/// given it, and does not wait for the rest.
#[must_use]
pub fn spoken() -> Vec<WsStep> {
    let chunk = tone();
    let first = timed(
        "\"It\",\"is\",\"ten\",\"past\",\"three.\"",
        "0.0,0.1,0.2,0.4,0.6",
        "0.1,0.2,0.4,0.6,0.8",
    );
    let second = timed("\"Goodbye!\"", "0.8", "1.2");
    let script = [
        // The first sentence.
        "> \"transcript\":\"It is",
        &chunk,
        &first,
        &chunk,
        // The second, which is given once the first has its audio.
        "> Goodbye!",
        &chunk,
        &second,
        // And once the adapter has said that no more text follows.
        "> \"continue\":false",
        DONE,
    ]
    .join("\n");
    WsStep::script(&script)
}

/// What the synthesiser says for [`GREETING`]: 400 ms of the same tone,
/// with where the two words are.
#[must_use]
pub fn greeted() -> Vec<WsStep> {
    let script = [
        "> \"transcript\":\"Good morning.",
        &tone(),
        &timed("\"Good\",\"morning.\"", "0.0,0.2", "0.2,0.4"),
        "> \"continue\":false",
        DONE,
    ]
    .join("\n");
    WsStep::script(&script)
}

#[cfg(test)]
mod tests {
    use super::{GREETING, HEARD, SAID, answer_text, greeted, spoken, unfinished};
    use crate::servers::{End, HttpAnswer, WsStep};

    #[test]
    fn an_answer_holds_its_words_and_says_how_it_ends() {
        let plain = answer_text(&SAID, None);
        for word in SAID {
            assert!(
                plain.contains(&format!("\"content\":\"{word}\"")),
                "{plain}"
            );
        }
        assert!(plain.contains("\"finish_reason\":\"stop\""), "{plain}");
        assert!(plain.ends_with("data: [DONE]\n\n"), "{plain}");
        let calls = answer_text(&[], Some("end_call"));
        assert!(calls.contains("\"name\":\"end_call\""), "{calls}");
        assert!(
            calls.contains("\"finish_reason\":\"tool_calls\""),
            "{calls}"
        );
        assert!(!calls.contains("\"finish_reason\":\"stop\""), "{calls}");
    }

    #[test]
    fn an_unfinished_answer_says_its_words_and_never_that_it_is_over() {
        let HttpAnswer::Stream { pieces, end, .. } = unfinished(&SAID[..1]) else {
            panic!("an answer that arrives in pieces is a stream");
        };
        assert_eq!(end, End::Hang);
        // Who writes, and the words: each a piece by itself.
        assert_eq!(pieces.len(), 2);
        let all = String::from_utf8(pieces.concat()).unwrap_or_default();
        assert!(all.contains("\"content\":\"It is\""), "{all}");
        assert!(
            !all.contains("[DONE]") && !all.contains("\"stop\""),
            "{all}"
        );
    }

    #[test]
    fn the_scripts_are_ones_a_server_can_play() {
        // What is heard is read as a script, with something in it.
        assert!(WsStep::script(HEARD).len() >= 3);
        assert_eq!(spoken().len(), 9);
        assert_eq!(greeted().len(), 5);
    }

    #[test]
    fn a_voice_waits_for_the_words_it_is_the_voice_of() {
        // Each begins with what it waits to be asked to say.
        let waits_for = |steps: &[WsStep], words: &str| {
            let first = format!("{:?}", steps.first());
            assert!(first.contains(words), "{first}");
        };
        waits_for(&greeted(), &GREETING[..GREETING.len() - 1]);
        waits_for(&spoken(), SAID[0]);
    }
}
