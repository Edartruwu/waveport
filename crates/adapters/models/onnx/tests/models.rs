//! The two models, loaded from the files `cargo xtask fetch-assets`
//! fetches, and given the speech of the test kit to listen to.
//!
//! The files are looked for in `target/assets/` of the repository, or in
//! the directory `WAVEPORT_ASSETS` names.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test that cannot be set up, or that finds something missing, fails"
)]

use std::path::PathBuf;

use waveport_models_onnx::{
    LIBRARY, Runtime, SileroState, SileroVad, SmartTurn, TURN_MODEL, VAD_MODEL,
};
use waveport_ports::audio::Rate;
use waveport_ports::error::RetryClass;
use waveport_ports::model::{TurnModel, VadModel};
use waveport_testkit::speech::{framed, what_time_is_it};

const RATE: Rate = Rate::Hz16000;

fn assets() -> PathBuf {
    std::env::var_os("WAVEPORT_ASSETS").map_or_else(
        || {
            PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../../../target/assets"
            ))
        },
        PathBuf::from,
    )
}

fn runtime() -> Runtime {
    match Runtime::load_from(&assets()) {
        Ok(runtime) => runtime,
        Err(error) => panic!("{error}"),
    }
}

fn vad() -> SileroVad {
    SileroVad::load(runtime(), &assets().join(VAD_MODEL)).unwrap()
}

fn turn() -> SmartTurn {
    SmartTurn::load(runtime(), &assets().join(TURN_MODEL)).unwrap()
}

/// Runs one stream through the model, a frame at a time, and returns how
/// likely speech was in each frame.
fn listen(vad: &mut SileroVad, audio: &[i16]) -> Vec<f32> {
    let mut state = [SileroState::default()];
    let mut likely = Vec::new();
    for frame in audio.chunks_exact(vad.frame()) {
        let mut answer = [0.0];
        vad.infer(&[frame], &mut state, &mut answer).unwrap();
        likely.extend(answer);
    }
    likely
}

#[test]
fn speech_is_told_from_the_silence_around_it() {
    let mut vad = vad();
    assert_eq!((vad.rate(), vad.frame()), (RATE, 512));
    // Half a second of silence, a question of a second and three
    // quarters, and half a second of silence.
    let audio = framed(RATE, 500, &what_time_is_it(RATE), 500);
    let likely = listen(&mut vad, &audio);
    assert_eq!(likely.len(), 85);

    let (before, rest) = likely.split_at(12);
    assert!(before.iter().all(|p| *p < 0.2), "{before:?}");
    let spoken = rest.iter().filter(|p| **p > 0.9).count();
    assert!(spoken >= 35, "{spoken} frames of speech in {rest:?}");
    let after = &likely[likely.len() - 8..];
    assert!(after.iter().all(|p| *p < 0.2), "{after:?}");
    // Speech is one stretch: once it is over it does not come back.
    let last = likely.iter().rposition(|p| *p > 0.5).unwrap();
    let first = likely.iter().position(|p| *p > 0.5).unwrap();
    let within = &likely[first..=last];
    assert!(within.iter().all(|p| *p > 0.5), "{within:?}");
}

#[test]
fn a_stream_is_judged_the_same_alone_and_among_others() {
    let mut vad = vad();
    let question = framed(RATE, 200, &what_time_is_it(RATE), 200);
    let alone = listen(&mut vad, &question);

    // The same stream with two others beside it: one of silence, and
    // one of the question begun a little later.
    let silence = vec![0_i16; question.len()];
    let later = framed(RATE, 328, &what_time_is_it(RATE), 72);
    let mut states = [
        SileroState::default(),
        SileroState::default(),
        SileroState::default(),
    ];
    let mut among = Vec::new();
    let mut others = Vec::new();
    let streams = [&silence, &question, &later];
    for n in 0..question.len() / 512 {
        let frames: Vec<&[i16]> = streams
            .iter()
            .map(|stream| &stream[n * 512..(n + 1) * 512])
            .collect();
        let mut answers = [0.0; 3];
        vad.infer(&frames, &mut states, &mut answers).unwrap();
        among.push(answers[1]);
        others.push((answers[0], answers[2]));
    }
    // To the last bit.
    let bits = |values: &[f32]| values.iter().map(|p| p.to_bits()).collect::<Vec<_>>();
    assert_eq!(bits(&among), bits(&alone));
    // And the others were judged for themselves.
    assert!(others.iter().all(|(silent, _)| *silent < 0.2));
    assert!(others.iter().filter(|(_, spoken)| *spoken > 0.9).count() >= 35);
}

#[test]
fn a_call_that_cannot_be_made_changes_nothing() {
    let mut vad = vad();
    let frame = [1_000_i16; 512];
    let mut states = [SileroState::default(), SileroState::default()];
    let mut answers = [0.0; 2];
    vad.infer(&[&frame, &frame], &mut states, &mut answers)
        .unwrap();
    let (before, answered) = (states.clone(), answers);

    // A frame of the wrong length.
    let short = [0_i16; 480];
    let error = vad
        .infer(&[&frame, &short], &mut states, &mut answers)
        .unwrap_err();
    assert_eq!(error.class, RetryClass::Permanent);
    assert!(error.message.contains("480"), "{error}");
    // Not as many states as frames, and then not as much room.
    assert!(vad.infer(&[&frame], &mut states, &mut answers).is_err());
    assert!(
        vad.infer(&[&frame, &frame], &mut states, &mut answers[..1])
            .is_err()
    );
    // More frames than a call takes.
    let many = vec![&frame[..]; vad.max_batch() + 1];
    let mut many_states = vec![SileroState::default(); many.len()];
    let mut many_answers = vec![0.0; many.len()];
    assert!(
        vad.infer(&many, &mut many_states, &mut many_answers)
            .is_err()
    );

    assert_eq!(states, before);
    assert_eq!(answers, answered);
    // No frames at all is nothing to do.
    vad.infer(&[], &mut [], &mut []).unwrap();
    // And as many as a call takes are taken.
    let full = vec![&frame[..]; vad.max_batch()];
    let mut full_states = vec![SileroState::default(); full.len()];
    let mut full_answers = vec![0.0; full.len()];
    vad.infer(&full, &mut full_states, &mut full_answers)
        .unwrap();
    assert!(
        full_answers
            .iter()
            .all(|p| p.to_bits() == full_answers[0].to_bits())
    );
}

#[test]
fn a_question_that_was_asked_is_told_from_one_that_was_begun() {
    let mut turn = turn();
    assert_eq!((turn.rate(), turn.window()), (RATE, 128_000));
    let question = what_time_is_it(RATE);
    // "What time is it right now?", and a pause.
    let asked = framed(RATE, 300, &question, 200);
    // "What time is", and "What time", each with the same pause.
    let begun = framed(RATE, 300, &question[..question.len() * 45 / 100], 200);
    let barely = framed(RATE, 300, &question[..question.len() * 30 / 100], 200);
    let mut likely = [0.0; 3];
    for (audio, answer) in [&asked, &begun, &barely].into_iter().zip(&mut likely) {
        turn.infer(&[audio], core::slice::from_mut(answer)).unwrap();
    }
    let [asked, begun, barely] = likely;
    assert!(asked > 0.8, "a whole question: {asked}");
    assert!(begun < 0.2, "half a question: {begun}");
    assert!(barely < 0.2, "the start of a question: {barely}");
}

#[test]
fn only_the_end_of_a_long_turn_is_looked_at() {
    let mut turn = turn();
    let question = what_time_is_it(RATE);
    let end = framed(RATE, 6_000, &question, 200);
    assert!(end.len() < turn.window());
    // Ten seconds of other talk before the same eight seconds.
    let mut long = Vec::new();
    for _ in 0..6 {
        long.extend_from_slice(&question);
    }
    long.extend_from_slice(&vec![0; turn.window() - end.len()]);
    long.extend_from_slice(&end);
    let mut likely = [0.0; 2];
    turn.infer(&[&long], &mut likely[..1]).unwrap();
    turn.infer(&[&end], &mut likely[1..]).unwrap();
    assert_eq!(likely[0].to_bits(), likely[1].to_bits());

    // As many answers as turns, or none.
    let error = turn.infer(&[&end, &end], &mut likely[..1]).unwrap_err();
    assert_eq!(error.class, RetryClass::Permanent);
    turn.infer(&[], &mut []).unwrap();
}

#[test]
fn the_library_is_loaded_once_and_from_one_file() {
    let first = runtime();
    let again = Runtime::load(&assets().join(LIBRARY)).unwrap();
    assert_eq!(first.library(), again.library());
    let error = Runtime::load(&assets().join("another").join(LIBRARY)).unwrap_err();
    assert_eq!(error.class, RetryClass::Permanent);
    assert!(error.message.contains("already loaded"), "{error}");
}

#[test]
fn a_model_that_is_not_there_says_how_to_get_it() {
    let runtime = runtime();
    let missing = assets().join("no-such-model.onnx");
    for error in [
        SileroVad::load(runtime, &missing).unwrap_err(),
        SmartTurn::load(runtime, &missing).unwrap_err(),
    ] {
        assert_eq!(error.class, RetryClass::Permanent);
        assert!(error.message.contains("fetch-assets"), "{error}");
    }
    // One model's file is not the other's.
    let error = SileroVad::load(runtime, &assets().join(TURN_MODEL)).unwrap_err();
    assert!(
        error.message.contains("not the voice-activity model"),
        "{error}"
    );
}
