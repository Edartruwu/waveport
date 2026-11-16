//! The conformance checks, run against the fakes, and against parts that
//! break a rule on purpose.
//!
//! The first half shows that the fakes keep the rules of their ports
//! whatever they are set up to be able to do, which is what makes them
//! fit to stand in for an adapter. The second half shows that the checks
//! check: each part there breaks one rule, and the check has to name it.

use core::pin::Pin;
use core::time::Duration;

use waveport_adapter_kit::stream;
use waveport_domain::time::{MediaDur, MediaSpan, MediaTime};
use waveport_ports::audio::{PcmBytes, Rate, RateSet};
use waveport_ports::error::{BoxFuture, Full, PortError};
use waveport_ports::journal::{
    Batch, Journal, JournalError, Lease, Recovered, Seq, SessionId, Snapshot,
};
use waveport_ports::llm::{Finish, Llm, LlmCaps, LlmEvent, LlmRequest, LlmRx, Message, ToolDecl};
use waveport_ports::stt::{
    Stt, SttCaps, SttConfig, SttEvent, SttFinalize, SttStream, SttTiming, SttTx, Transcript,
};
use waveport_ports::tts::{
    Alignment, Tts, TtsCaps, TtsConfig, TtsEvent, TtsInput, TtsMark, TtsStream, TtsTx,
};
use waveport_testkit::conformance::{
    Talk, journal_session, llm_abandoned, llm_response, stt_utterance, tts_abandoned, tts_utterance,
};
use waveport_testkit::fakes::{
    LlmStep, MemoryJournal, ScriptedLlm, ScriptedStt, ScriptedTts, Spoken, SttScript, TtsScript,
};

// ---------------------------------------------------------------------
// Language models
// ---------------------------------------------------------------------

const TOOLS: [ToolDecl<'static>; 2] = [
    ToolDecl {
        name: "get_time",
        description: "The time of day.",
        parameters: "{\"type\":\"object\"}",
    },
    ToolDecl {
        name: "end_call",
        description: "Ends the call.",
        parameters: "{\"type\":\"object\"}",
    },
];

const MESSAGES: [Message<'static>; 1] = [Message::User {
    speaker: None,
    text: "What time is it?",
}];

const REQUEST: LlmRequest<'static> = LlmRequest {
    instructions: "You are a helpful agent.",
    messages: &MESSAGES,
    tools: &TOOLS,
    max_output_tokens: None,
};

#[tokio::test(start_paused = true)]
async fn a_scripted_model_keeps_the_rules() {
    let llm = ScriptedLlm::replies(vec![
        vec![LlmStep::say("It is a quarter past nine.")],
        vec![
            LlmStep::say("Let me look. "),
            LlmStep::call("get_time", "{}"),
            LlmStep::call("end_call", "{\"why\":\"done\"}"),
        ],
        vec![
            LlmStep::Delta("Cut".to_owned()),
            LlmStep::Stop(Finish::Length),
        ],
    ]);

    let seen = llm_response(&llm, REQUEST).await.unwrap();
    assert_eq!(seen.text, "It is a quarter past nine.");
    assert_eq!((seen.calls.len(), seen.finish), (0, Finish::Stop));
    assert_eq!(seen.usage.unwrap().output, 6, "a word at a time");

    let seen = llm_response(&llm, REQUEST).await.unwrap();
    assert_eq!(seen.text, "Let me look. ");
    assert_eq!(
        seen.calls,
        [
            ("get_time".to_owned(), "{}".to_owned()),
            ("end_call".to_owned(), "{\"why\":\"done\"}".to_owned()),
        ]
    );
    assert_eq!(seen.finish, Finish::ToolCalls);

    let seen = llm_response(&llm, REQUEST).await.unwrap();
    assert_eq!((seen.text.as_str(), seen.finish), ("Cut", Finish::Length));

    // The model was shown what it was asked.
    let asked = llm.asked();
    assert_eq!(asked.len(), 3);
    assert_eq!(asked[0].instructions, "You are a helpful agent.");
    assert_eq!(asked[0].last_user(), Some("What time is it?"));
    assert_eq!(asked[0].tools, ["get_time", "end_call"]);
}

#[tokio::test(start_paused = true)]
async fn a_scripted_model_that_is_cut_off_is_as_good_as_new() {
    let llm = ScriptedLlm::with(|asked| {
        vec![LlmStep::Say(format!(
            "You asked: {}",
            asked.last_user().unwrap_or_default()
        ))]
    });
    let seen = llm_abandoned(&llm, REQUEST).await.unwrap();
    assert_eq!(seen.text, "You asked: What time is it?");
    assert_eq!(llm.asked().len(), 2);
}

/// A model that gives exactly these events, whatever the rules say.
struct RawLlm {
    caps: LlmCaps,
    events: Vec<Result<LlmEvent, PortError>>,
    /// Whether it then stays silent instead of ending.
    hangs: bool,
}

impl RawLlm {
    fn new(events: Vec<LlmEvent>) -> Self {
        Self {
            caps: LlmCaps {
                tools: true,
                parallel_tools: true,
            },
            events: events.into_iter().map(Ok).collect(),
            hangs: false,
        }
    }
}

impl Llm for RawLlm {
    fn caps(&self) -> LlmCaps {
        self.caps
    }

    fn start<'a>(
        &'a self,
        _request: LlmRequest<'a>,
    ) -> BoxFuture<'a, Result<Pin<Box<dyn LlmRx>>, PortError>> {
        let (given, hangs) = (self.events.clone(), self.hangs);
        Box::pin(async move {
            let rx = stream::events(move |mut events| async move {
                for event in given {
                    events.send(event?).await;
                }
                if hangs {
                    core::future::pending::<()>().await;
                }
                Ok(())
            });
            let rx: Pin<Box<dyn LlmRx>> = Box::pin(rx);
            Ok(rx)
        })
    }
}

fn delta(text: &str) -> LlmEvent {
    LlmEvent::Delta {
        text: text.to_owned(),
    }
}

fn call(tool: &str) -> LlmEvent {
    LlmEvent::ToolCall {
        tool: tool.to_owned(),
        args: "{}".to_owned(),
    }
}

const fn done(finish: Finish) -> LlmEvent {
    LlmEvent::Done {
        finish,
        usage: None,
    }
}

#[tokio::test(start_paused = true)]
async fn a_model_that_breaks_a_rule_is_told_which() {
    let no_tools = LlmCaps {
        tools: false,
        parallel_tools: false,
    };
    let one_at_a_time = LlmCaps {
        tools: true,
        parallel_tools: false,
    };
    let cases: Vec<(RawLlm, &str)> = vec![
        (
            RawLlm::new(vec![delta("Hello"), delta(""), done(Finish::Stop)]),
            "a piece of text is empty",
        ),
        (
            RawLlm::new(vec![delta("Hello")]),
            "ended without saying why",
        ),
        (
            RawLlm::new(vec![delta("Hello"), done(Finish::Stop), delta(" again")]),
            "something followed the end",
        ),
        (
            RawLlm::new(vec![call("launch_rocket"), done(Finish::ToolCalls)]),
            "`launch_rocket`, which was not offered",
        ),
        (
            RawLlm {
                caps: no_tools,
                ..RawLlm::new(vec![call("get_time"), done(Finish::ToolCalls)])
            },
            "says it cannot call tools",
        ),
        (
            RawLlm {
                caps: one_at_a_time,
                ..RawLlm::new(vec![
                    call("get_time"),
                    call("end_call"),
                    done(Finish::ToolCalls),
                ])
            },
            "one call at a time asked for several",
        ),
        (
            RawLlm::new(vec![call("get_time"), done(Finish::Stop)]),
            "asked for 1 tool calls and stopped with Stop",
        ),
        (
            RawLlm::new(vec![delta("Hello"), done(Finish::ToolCalls)]),
            "asked for 0 tool calls and stopped with ToolCalls",
        ),
        (
            RawLlm {
                hangs: true,
                ..RawLlm::new(vec![delta("Hello")])
            },
            "did not arrive within 30 s",
        ),
        (
            RawLlm {
                events: vec![
                    Ok(delta("Hello")),
                    Err(PortError::transient("the connection dropped")),
                ],
                ..RawLlm::new(Vec::new())
            },
            "the response failed: transient: the connection dropped",
        ),
    ];
    for (llm, rule) in cases {
        let broken = llm_response(&llm, REQUEST).await.unwrap_err();
        assert!(broken.0.contains(rule), "expected `{rule}`, got `{broken}`");
    }
}

// ---------------------------------------------------------------------
// Speech recognition
// ---------------------------------------------------------------------

const RATE: Rate = Rate::Hz16000;

/// Two and a half seconds of audio from 1,000 ms on. What is in it does
/// not matter to a scripted recogniser.
fn talk(samples: &[i16]) -> Talk<'_> {
    Talk {
        at: MediaTime::from_millis(1_000),
        rate: RATE,
        samples,
        paced: false,
    }
}

fn spoken() -> Vec<Spoken> {
    vec![Spoken::new(1_200, 2_400, "what is the weather in Lima")]
}

#[tokio::test(start_paused = true)]
async fn a_scripted_recogniser_keeps_the_rules_whatever_it_can_do() {
    let samples = vec![0_i16; RATE.frame() * 125];
    for interim in [true, false] {
        for timing in [SttTiming::Words, SttTiming::Window] {
            for finalize in [
                SttFinalize::OnRequest,
                SttFinalize::ByProvider,
                SttFinalize::Both,
            ] {
                let caps = SttCaps {
                    rates: RateSet::of(&[RATE]),
                    interim,
                    timing,
                    finalize,
                };
                let stt = ScriptedStt::new(SttScript {
                    caps,
                    ..SttScript::new(spoken())
                });
                let seen = stt_utterance(&stt, talk(&samples))
                    .await
                    .unwrap_or_else(|broken| panic!("{caps:?}: {broken}"));
                assert_eq!(seen.text(), "what is the weather in Lima", "{caps:?}");
                assert_eq!(seen.interim.len(), if interim { 6 } else { 0 }, "{caps:?}");

                // Where the words are said to be.
                let span = seen.finals[0].span;
                let expected = match timing {
                    SttTiming::Words => {
                        MediaSpan::new(MediaTime::from_millis(1_200), MediaTime::from_millis(2_400))
                    }
                    // The audio sent until the transcript was made.
                    SttTiming::Window => MediaSpan::new(MediaTime::from_millis(1_000), span.end),
                };
                assert_eq!(span, expected, "{caps:?}");

                let log = stt.log();
                assert_eq!(log.opened, [SttConfig::new(RATE)]);
                assert_eq!(log.audio_ticks, 48 * 2_500);
                let asked = usize::from(finalize.on_request());
                assert_eq!(log.finalize_requests, asked, "{caps:?}");
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_recogniser_that_decides_for_itself_needs_no_request() {
    let samples = vec![0_i16; RATE.frame() * 125];
    let stt = ScriptedStt::new(SttScript {
        endpoint: MediaDur::from_millis(500),
        ..SttScript::new(spoken())
    });
    let mut stream = stt.open(SttConfig::new(RATE)).await.unwrap();
    let mut at = MediaTime::from_millis(1_000);
    let mut finals = Vec::new();
    for piece in samples.chunks(RATE.frame()) {
        let audio = waveport_ports::audio::AudioRef {
            at,
            rate: RATE,
            samples: piece,
        };
        stream.tx.try_send(audio).unwrap();
        at = audio.span().end;
    }
    drop(stream.tx);
    while let Some(event) = waveport_testkit::drive::next_stt(&mut stream.rx).await {
        let SttEvent::Transcript(transcript) = event.unwrap();
        if transcript.is_final {
            finals.push(transcript.text);
        }
    }
    // Half a second after the last word, without being asked.
    assert_eq!(finals, ["what is the weather in Lima"]);
    assert_eq!(stt.log().finalize_requests, 0);
}

#[tokio::test(start_paused = true)]
async fn a_scripted_recogniser_fails_as_it_is_told_to() {
    let samples = vec![0_i16; RATE.frame() * 125];
    let stt = ScriptedStt::new(SttScript {
        fail_open: Some(PortError::auth("the key was refused")),
        ..SttScript::new(spoken())
    });
    let broken = stt_utterance(&stt, talk(&samples)).await.unwrap_err();
    assert!(
        broken.0.contains("did not open: not authorised"),
        "{broken}"
    );

    let stt = ScriptedStt::new(SttScript {
        fail_after: Some((2, PortError::transient("the connection dropped"))),
        ..SttScript::new(spoken())
    });
    let broken = stt_utterance(&stt, talk(&samples)).await.unwrap_err();
    assert!(
        broken.0.contains("the stream failed: transient"),
        "{broken}"
    );

    let broken = stt_utterance(
        &stt,
        Talk {
            rate: Rate::Hz8000,
            ..talk(&samples)
        },
    )
    .await
    .unwrap_err();
    assert!(broken.0.contains("does not take"), "{broken}");
}

/// A recogniser that gives exactly these transcripts when it is asked to
/// finalise, whatever the rules say.
struct RawStt {
    caps: SttCaps,
    transcripts: Vec<Transcript>,
    /// Whether it takes audio at all.
    takes_audio: bool,
    /// Whether its stream ends when the sending half is dropped.
    ends: bool,
    /// Whether its stream ends as soon as it has answered.
    quits: bool,
}

impl RawStt {
    fn new(transcripts: Vec<Transcript>) -> Self {
        Self {
            caps: SttCaps {
                rates: RateSet::of(&[RATE]),
                interim: false,
                timing: SttTiming::Words,
                finalize: SttFinalize::OnRequest,
            },
            transcripts,
            takes_audio: true,
            ends: true,
            quits: false,
        }
    }
}

struct RawSttTx {
    tx: stream::Sender<bool>,
    takes_audio: bool,
}

impl SttTx for RawSttTx {
    fn try_send(&mut self, _audio: waveport_ports::audio::AudioRef<'_>) -> Result<(), Full> {
        if self.takes_audio { Ok(()) } else { Err(Full) }
    }

    fn try_finalize(&mut self) -> Result<(), Full> {
        self.tx.try_send(true)
    }
}

impl Stt for RawStt {
    fn caps(&self) -> SttCaps {
        self.caps
    }

    fn open(&self, _config: SttConfig) -> BoxFuture<'_, Result<SttStream, PortError>> {
        let (transcripts, ends, quits) = (self.transcripts.clone(), self.ends, self.quits);
        let takes_audio = self.takes_audio;
        Box::pin(async move {
            let (tx, rx) = stream::pair(4, move |mut commands, mut events| async move {
                if commands.next().await.is_some() {
                    for transcript in transcripts {
                        events.send(SttEvent::Transcript(transcript)).await;
                    }
                }
                if quits {
                    return Ok(());
                }
                while commands.next().await.is_some() {}
                if !ends {
                    core::future::pending::<()>().await;
                }
                Ok(())
            });
            Ok(SttStream {
                tx: Box::new(RawSttTx { tx, takes_audio }),
                rx: Box::pin(rx),
            })
        })
    }
}

fn transcript(from_ms: u64, to_ms: u64, is_final: bool) -> Transcript {
    Transcript {
        span: MediaSpan::new(
            MediaTime::from_millis(from_ms),
            MediaTime::from_millis(to_ms),
        ),
        text: "words".to_owned(),
        is_final,
    }
}

#[tokio::test(start_paused = true)]
async fn a_recogniser_that_breaks_a_rule_is_told_which() {
    let samples = vec![0_i16; RATE.frame() * 125];
    let good = transcript(1_200, 2_400, true);
    let cases: Vec<(RawStt, &str)> = vec![
        (
            RawStt::new(vec![transcript(1_200, 1_800, false), good.clone()]),
            "says it gives no interim transcripts gave one",
        ),
        (
            RawStt::new(vec![transcript(200, 900, true)]),
            "outside the audio that was sent",
        ),
        (
            RawStt::new(vec![transcript(1_200, 9_000, true)]),
            "outside the audio that was sent",
        ),
        (
            RawStt::new(vec![
                transcript(2_000, 2_400, true),
                transcript(1_200, 1_900, true),
            ]),
            "begins before the one before it",
        ),
        (
            RawStt::new(Vec::new()),
            "the final transcript that was asked for did not arrive within 30 s",
        ),
        (
            RawStt {
                quits: true,
                ..RawStt::new(Vec::new())
            },
            "ended without a final transcript",
        ),
        (
            RawStt {
                takes_audio: false,
                ..RawStt::new(vec![good.clone()])
            },
            "audio was refused",
        ),
        (
            RawStt {
                ends: false,
                ..RawStt::new(vec![good.clone()])
            },
            "the end of the stream did not arrive within 30 s",
        ),
    ];
    for (stt, rule) in cases {
        let broken = stt_utterance(&stt, talk(&samples)).await.unwrap_err();
        assert!(broken.0.contains(rule), "expected `{rule}`, got `{broken}`");
    }
    // And one that keeps them all passes.
    let seen = stt_utterance(&RawStt::new(vec![good]), talk(&samples))
        .await
        .unwrap();
    assert_eq!(seen.text(), "words");
}

// ---------------------------------------------------------------------
// Speech synthesis
// ---------------------------------------------------------------------

const PIECES: [&str; 2] = ["Hello there. ", "How can I h\u{e9}lp?"];

#[tokio::test(start_paused = true)]
async fn a_scripted_synthesiser_keeps_the_rules_whatever_it_can_do() {
    // 28 characters at 60 ms each.
    let length = MediaDur::from_millis(28 * 60);
    for rate in [Rate::Hz16000, Rate::Hz24000] {
        for input in [TtsInput::Incremental, TtsInput::Whole] {
            for (alignment, marks) in [
                // The end of each piece, when each is spoken on its own.
                (
                    Alignment::None,
                    if input == TtsInput::Whole { 2 } else { 0 },
                ),
                (Alignment::Word, 6),
                (Alignment::Char, 28),
            ] {
                let caps = TtsCaps {
                    rates: RateSet::of(&[Rate::Hz16000, Rate::Hz24000]),
                    input,
                    alignment,
                };
                let tts = ScriptedTts::new(TtsScript {
                    caps,
                    ..TtsScript::default()
                });
                let seen = tts_utterance(&tts, rate, &PIECES)
                    .await
                    .unwrap_or_else(|broken| panic!("{caps:?}: {broken}"));
                assert_eq!(seen.length, length, "{caps:?}");
                assert_eq!(seen.samples.len(), rate.samples_in(length));
                assert!(seen.samples.iter().all(|sample| sample.abs() >= 2_000));
                assert_eq!(seen.marks.len(), marks, "{caps:?}");
                if let Some(last) = seen.marks.last() {
                    // The last mark is the end of the text and of the audio.
                    assert_eq!(
                        *last,
                        TtsMark {
                            text_end: 29,
                            audio_end: length
                        },
                        "{caps:?}"
                    );
                }
                assert_eq!(tts.log().spoken, [PIECES.map(str::to_owned).to_vec()]);
                assert_eq!(tts.log().finished, 1);
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_scripted_synthesiser_marks_each_word_where_its_audio_ends() {
    let tts = ScriptedTts::default();
    let seen = tts_utterance(&tts, Rate::Hz24000, &["One two. ", "Three"])
        .await
        .unwrap();
    let marks: Vec<(u32, u32)> = seen
        .marks
        .iter()
        .map(|mark| (mark.text_end, mark.audio_end.as_millis()))
        .collect();
    // "One " is 4 characters, "two. " 5 more, "Three" 5 more.
    assert_eq!(marks, [(4, 240), (9, 540), (14, 840)]);
}

#[tokio::test(start_paused = true)]
async fn a_scripted_synthesiser_is_faster_than_speech_and_can_be_cut_off() {
    let tts = ScriptedTts::default();
    let started = tokio::time::Instant::now();
    let seen = tts_abandoned(&tts, Rate::Hz24000, &PIECES).await.unwrap();
    // 1,680 ms of audio, made four times as fast as it is spoken, after
    // 100 ms before the first of it, twice over for the stream cut off.
    let took = started.elapsed();
    assert_eq!(seen.length.as_millis(), 1_680);
    assert!(took < Duration::from_millis(700), "{took:?}");
    assert!(took > Duration::from_millis(400), "{took:?}");
    let log = tts.log();
    assert_eq!((log.spoken.len(), log.finished), (2, 1));

    let tts = ScriptedTts::new(TtsScript {
        fail_after: Some((3, PortError::transient("the connection dropped"))),
        ..TtsScript::default()
    });
    let broken = tts_utterance(&tts, Rate::Hz24000, &PIECES)
        .await
        .unwrap_err();
    assert!(
        broken.0.contains("the stream failed: transient"),
        "{broken}"
    );
    let broken = tts_utterance(&tts, Rate::Hz48000, &PIECES)
        .await
        .unwrap_err();
    assert!(broken.0.contains("does not give"), "{broken}");
}

/// A synthesiser that gives exactly these events, whatever the rules
/// say.
struct RawTts {
    alignment: Alignment,
    events: Vec<TtsEvent>,
}

struct RawTtsTx;

impl TtsTx for RawTtsTx {
    fn try_speak(&mut self, _text: &str) -> Result<(), Full> {
        Ok(())
    }

    fn try_finish(&mut self) -> Result<(), Full> {
        Ok(())
    }
}

impl Tts for RawTts {
    fn caps(&self) -> TtsCaps {
        TtsCaps {
            rates: RateSet::of(&[Rate::Hz24000]),
            input: TtsInput::Incremental,
            alignment: self.alignment,
        }
    }

    fn open(&self, _config: TtsConfig) -> BoxFuture<'_, Result<TtsStream, PortError>> {
        let given = self.events.clone();
        Box::pin(async move {
            let rx = stream::events(move |mut events| async move {
                for event in given {
                    events.send(event).await;
                }
                Ok(())
            });
            Ok(TtsStream {
                tx: Box::new(RawTtsTx),
                rx: Box::pin(rx),
            })
        })
    }
}

/// A second of audio.
fn audio() -> TtsEvent {
    TtsEvent::Audio(PcmBytes::from_samples(&vec![1_000; 24_000]))
}

const fn mark(text_end: u32, audio_ms: u32) -> TtsEvent {
    TtsEvent::Mark(TtsMark {
        text_end,
        audio_end: MediaDur::from_millis(audio_ms),
    })
}

#[tokio::test(start_paused = true)]
async fn a_synthesiser_that_breaks_a_rule_is_told_which() {
    let empty = TtsEvent::Audio(PcmBytes::default());
    let cases: Vec<(Alignment, Vec<TtsEvent>, &str)> = vec![
        (Alignment::None, vec![TtsEvent::Done], "no audio arrived"),
        (
            Alignment::None,
            vec![empty, TtsEvent::Done],
            "a piece of audio is empty",
        ),
        (
            Alignment::None,
            vec![audio()],
            "ended without saying that it was done",
        ),
        (
            Alignment::None,
            vec![audio(), TtsEvent::Done, audio()],
            "something followed the end",
        ),
        (
            Alignment::Word,
            vec![mark(13, 500), mark(6, 600), audio(), TtsEvent::Done],
            "a mark goes back",
        ),
        (
            Alignment::Word,
            vec![mark(6, 500), mark(13, 400), audio(), TtsEvent::Done],
            "a mark goes back",
        ),
        (
            Alignment::Word,
            vec![mark(29, 1_500), audio(), TtsEvent::Done],
            "points at 1500 ms of audio, and there are 1000 ms",
        ),
        (
            // Byte 25 is in the middle of the é.
            Alignment::Char,
            vec![mark(25, 900), audio(), TtsEvent::Done],
            "not between two characters",
        ),
        (
            Alignment::Word,
            vec![mark(99, 900), audio(), TtsEvent::Done],
            "not between two characters",
        ),
        (
            Alignment::Word,
            vec![audio(), TtsEvent::Done],
            "gave marks up to byte 0",
        ),
        (
            // The last word, "hélp?", begins at byte 23.
            Alignment::Word,
            vec![mark(13, 400), mark(23, 900), audio(), TtsEvent::Done],
            "gave marks up to byte 23, and the last word begins at byte 23",
        ),
    ];
    for (alignment, events, rule) in cases {
        let tts = RawTts { alignment, events };
        let broken = tts_utterance(&tts, Rate::Hz24000, &PIECES)
            .await
            .unwrap_err();
        assert!(broken.0.contains(rule), "expected `{rule}`, got `{broken}`");
    }
    // And one that keeps them all passes.
    let tts = RawTts {
        alignment: Alignment::Word,
        events: vec![mark(13, 400), audio(), mark(29, 1_000), TtsEvent::Done],
    };
    let seen = tts_utterance(&tts, Rate::Hz24000, &PIECES).await.unwrap();
    assert_eq!((seen.marks.len(), seen.length.as_millis()), (2, 1_000));
}

// ---------------------------------------------------------------------
// The journal
// ---------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_memory_journal_keeps_the_rules() {
    let journal = MemoryJournal::new();
    journal_session(&journal, SessionId(1)).await.unwrap();
    // Sessions do not touch each other, and a clone is the same store.
    journal_session(&journal.clone(), SessionId(2))
        .await
        .unwrap();
    assert_eq!(journal.records(SessionId(1)).len(), 3, "4, 5 and 6");
    assert_eq!(journal.records(SessionId(3)), Vec::<bytes::Bytes>::new());

    // A session that is not new is not one to check with.
    let broken = journal_session(&journal, SessionId(1)).await.unwrap_err();
    assert!(broken.0.contains("is not new"), "{broken}");
}

#[tokio::test(start_paused = true)]
async fn a_store_that_fails_stores_nothing() {
    let journal = MemoryJournal::new();
    let session = SessionId(7);
    let lease = journal.acquire(session).await.unwrap().lease;
    let batch = |first| Batch {
        first: Seq(first),
        records: vec![bytes::Bytes::from_static(b"a record")],
    };
    assert_eq!(journal.append(session, lease, batch(1)).await, Ok(Seq(1)));
    journal.fail_next_append(PortError::transient("the disk is busy"));
    assert_eq!(
        journal.append(session, lease, batch(2)).await,
        Err(JournalError::Store(PortError::transient(
            "the disk is busy"
        )))
    );
    // The same records again are taken.
    assert_eq!(journal.append(session, lease, batch(2)).await, Ok(Seq(2)));
    // A session nobody has begun has no owner to write as.
    assert_eq!(
        journal.append(SessionId(8), lease, batch(1)).await,
        Err(JournalError::Fenced)
    );
}

/// A journal with one of its rules taken out.
struct Lax {
    inner: MemoryJournal,
    /// Lets an earlier owner write.
    no_fence: bool,
    /// Takes records whatever their numbers.
    any_order: bool,
    /// Forgets the snapshot when the session is taken over.
    drops_snapshot: bool,
}

impl Lax {
    fn new() -> Self {
        Self {
            inner: MemoryJournal::new(),
            no_fence: false,
            any_order: false,
            drops_snapshot: false,
        }
    }

    /// The lease and the next number the store really has. Taking the
    /// session over is the only way to ask.
    async fn current(&self, session: SessionId) -> Result<(Lease, Seq), JournalError> {
        let recovered = self.inner.acquire(session).await?;
        Ok((recovered.lease, recovered.next))
    }
}

impl Journal for Lax {
    fn acquire(&self, session: SessionId) -> BoxFuture<'_, Result<Recovered, JournalError>> {
        Box::pin(async move {
            let mut recovered = self.inner.acquire(session).await?;
            if self.drops_snapshot {
                recovered.snapshot = None;
            }
            Ok(recovered)
        })
    }

    fn append(
        &self,
        session: SessionId,
        lease: Lease,
        mut batch: Batch,
    ) -> BoxFuture<'_, Result<Seq, JournalError>> {
        Box::pin(async move {
            let mut lease = lease;
            if self.no_fence || self.any_order {
                // Whoever asks writes as the owner, and, when the order
                // does not matter either, at whatever number is next.
                let (current, next) = self.current(session).await?;
                lease = current;
                if self.any_order {
                    batch.first = next;
                }
            }
            self.inner.append(session, lease, batch).await
        })
    }

    fn snapshot(
        &self,
        session: SessionId,
        lease: Lease,
        snapshot: Snapshot,
    ) -> BoxFuture<'_, Result<(), JournalError>> {
        Box::pin(async move {
            let mut lease = lease;
            if self.no_fence {
                lease = self.current(session).await?.0;
            }
            self.inner.snapshot(session, lease, snapshot).await
        })
    }
}

#[tokio::test(start_paused = true)]
async fn a_journal_that_breaks_a_rule_is_told_which() {
    let cases = [
        (
            Lax {
                any_order: true,
                ..Lax::new()
            },
            "record 5 after record 3 was answered with Ok",
        ),
        (
            Lax {
                drops_snapshot: true,
                ..Lax::new()
            },
            "the snapshot did not come back",
        ),
        (
            Lax {
                no_fence: true,
                ..Lax::new()
            },
            "the earlier owner's append was answered with Ok",
        ),
    ];
    for (journal, rule) in cases {
        let broken = journal_session(&journal, SessionId(1)).await.unwrap_err();
        assert!(broken.0.contains(rule), "expected `{rule}`, got `{broken}`");
    }
    journal_session(&Lax::new(), SessionId(1)).await.unwrap();
}
