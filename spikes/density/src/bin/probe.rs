//! Day-one probe: do both backends load both models, do they agree, and
//! what does one inference cost?
//!
//! ```sh
//! cargo run --release --bin probe -- --assets ../../target/assets \
//!     --audio ../../target/spike --cpu 6 --out probe.json
//! ```

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use density_spike::backend::{self, Kind, VAD_CONTEXT, VAD_FRAME, VAD_INPUT, VAD_STATE};
use density_spike::mel::{FEATURES, MelFrontEnd, SAMPLE_RATE};
use density_spike::stats::{Samples, calibrate};
use density_spike::{Options, Result, audio, pin_to};

const VAD_MODELS: [&str; 2] = ["silero-vad-v6.2.3.onnx", "silero-vad-v6.2.3-ifless.onnx"];
const TURN_MODEL: &str = "smart-turn-v3.2-cpu.onnx";
const BACKENDS: [Kind; 2] = [Kind::Ort, Kind::Tract];

/// The clip cut into voice-activity frames, starting `skip` frames in and
/// wrapping around at the end.
fn frame_at(clip: &[f32], index: usize) -> &[f32] {
    let frames = clip.len() / VAD_FRAME;
    let at = (index % frames) * VAD_FRAME;
    &clip[at..at + VAD_FRAME]
}

/// Runs `streams` independent streams for `steps` frames in one batch and
/// returns the probabilities, step by step. Stream `i` starts `i * 7`
/// frames into the clip.
fn run_streams(
    vad: &mut dyn backend::Vad,
    clip: &[f32],
    streams: usize,
    steps: usize,
) -> Result<Vec<f32>> {
    let mut input = vec![0.0_f32; streams * VAD_INPUT];
    let mut state = vec![0.0_f32; streams * VAD_STATE];
    let mut probs = vec![0.0_f32; streams];
    let mut all = Vec::with_capacity(streams * steps);
    for step in 0..steps {
        for stream in 0..streams {
            let row = &mut input[stream * VAD_INPUT..(stream + 1) * VAD_INPUT];
            // The last samples of the previous frame become the context.
            row.copy_within(VAD_FRAME.., 0);
            row[VAD_CONTEXT..].copy_from_slice(frame_at(clip, step + stream * 7));
        }
        vad.run(streams, &input, &mut state, &mut probs)?;
        all.extend_from_slice(&probs);
    }
    Ok(all)
}

fn max_and_mean_diff(a: &[f32], b: &[f32]) -> (f32, f32) {
    let mut max = 0.0_f32;
    let mut sum = 0.0_f64;
    for (x, y) in a.iter().zip(b) {
        let diff = (x - y).abs();
        max = max.max(diff);
        sum += f64::from(diff);
    }
    (max, (sum / a.len().max(1) as f64) as f32)
}

/// Times `calls` batched inferences after a warm-up; samples are per call.
fn time_vad(
    vad: &mut dyn backend::Vad,
    clip: &[f32],
    batch: usize,
    calls: usize,
) -> Result<Samples> {
    let mut input = vec![0.0_f32; batch * VAD_INPUT];
    let mut state = vec![0.0_f32; batch * VAD_STATE];
    let mut probs = vec![0.0_f32; batch];
    let mut samples = Samples::with_capacity(calls);
    let warm = 50;
    for step in 0..warm + calls {
        for stream in 0..batch {
            let row = &mut input[stream * VAD_INPUT..(stream + 1) * VAD_INPUT];
            row.copy_within(VAD_FRAME.., 0);
            row[VAD_CONTEXT..].copy_from_slice(frame_at(clip, step + stream * 7));
        }
        let start = Instant::now();
        vad.run(batch, &input, &mut state, &mut probs)?;
        if step >= warm {
            samples.record_duration(start.elapsed());
        }
    }
    Ok(samples)
}

struct Report {
    json: String,
}

impl Report {
    fn field(&mut self, text: &str) {
        if !self.json.ends_with('{') && !self.json.ends_with('[') {
            self.json.push(',');
        }
        self.json.push_str(text);
    }
}

fn main() -> Result<()> {
    let options = Options::from_env();
    let assets = PathBuf::from(options.value("--assets").unwrap_or("../../target/assets"));
    let audio_dir = PathBuf::from(options.value("--audio").unwrap_or("../../target/spike"));
    let cpu: usize = options.parse("--cpu", 6)?;
    pin_to(cpu)?;

    let clip = audio::read_wav(&audio_dir.join("speech16k.wav"))?;
    if clip.rate as usize != SAMPLE_RATE {
        return Err("speech16k.wav is not at 16 kHz".into());
    }
    let clip = clip.to_f32();
    let frames = clip.len() / VAD_FRAME;
    let calibration_before = calibrate();
    println!(
        "pinned to CPU {cpu}; clip {:.1} s, {frames} frames",
        clip.len() as f64 / 16_000.0
    );

    let mut report = Report {
        json: String::from("{"),
    };
    report.field(&format!("\"cpu\":{cpu}"));
    report.field(&format!(
        "\"allocator\":\"{}\"",
        density_spike::alloc::name()
    ));

    // --- What the models declare -------------------------------------
    for name in VAD_MODELS.iter().chain([&TURN_MODEL]) {
        println!("\n{name}");
        #[cfg(feature = "backend-ort")]
        for line in backend::ort::describe(&assets.join(name))? {
            println!("  {line}");
        }
    }

    // --- Voice activity: load, agree, cost ------------------------------
    report.field("\"vad\":[");
    let mut reference: Option<Vec<f32>> = None;
    for model in VAD_MODELS {
        let path = assets.join(model);
        for kind in BACKENDS {
            let label = format!("{} / {model}", kind.name());
            let start = Instant::now();
            let mut vad = match backend::load_vad(kind, &path) {
                Ok(vad) => vad,
                Err(error) => {
                    println!("\n{label}: DOES NOT LOAD: {error}");
                    report.field(&format!(
                        "{{\"backend\":\"{}\",\"model\":\"{model}\",\"loads\":false}}",
                        kind.name()
                    ));
                    continue;
                }
            };
            let load_ms = start.elapsed().as_secs_f64() * 1e3;

            let single = match run_streams(vad.as_mut(), &clip, 1, frames) {
                Ok(single) => single,
                Err(error) => {
                    println!("\n{label}: loads but DOES NOT RUN: {error}");
                    report.field(&format!(
                        "{{\"backend\":\"{}\",\"model\":\"{model}\",\"loads\":true,\"runs\":false}}",
                        kind.name()
                    ));
                    continue;
                }
            };
            let speech = single.iter().filter(|&&p| p > 0.5).count();
            let reference = reference.get_or_insert_with(|| single.clone());
            let (max_diff, mean_diff) = max_and_mean_diff(reference, &single);
            println!(
                "\n{label}: loaded in {load_ms:.0} ms; speech in {speech}/{frames} frames; \
                 against the reference: max diff {max_diff:.2e}, mean {mean_diff:.2e}"
            );

            let mut entry = format!(
                "{{\"backend\":\"{}\",\"model\":\"{model}\",\"loads\":true,\"runs\":true,\
                 \"load_ms\":{load_ms:.1},\"speech_frames\":{speech},\"frames\":{frames},\
                 \"max_diff_vs_reference\":{max_diff:e},\"mean_diff_vs_reference\":{mean_diff:e},\
                 \"batches\":[",
                kind.name()
            );
            for (i, batch) in [1_usize, 8, 16, 32, 64].into_iter().enumerate() {
                // Batching must not change any stream's result.
                let steps = 120;
                let batched = run_streams(vad.as_mut(), &clip, batch, steps)?;
                let mut worst = 0.0_f32;
                for stream in [0, batch / 2, batch - 1] {
                    let mut alone_state = vec![0.0_f32; VAD_STATE];
                    let mut alone_input = vec![0.0_f32; VAD_INPUT];
                    let mut prob = [0.0_f32];
                    for step in 0..steps {
                        alone_input.copy_within(VAD_FRAME.., 0);
                        alone_input[VAD_CONTEXT..]
                            .copy_from_slice(frame_at(&clip, step + stream * 7));
                        vad.run(1, &alone_input, &mut alone_state, &mut prob)?;
                        worst = worst.max((prob[0] - batched[step * batch + stream]).abs());
                    }
                }
                let calls = if batch >= 32 { 300 } else { 600 };
                let summary = time_vad(vad.as_mut(), &clip, batch, calls)?.summary();
                let per_frame = summary.p50 as f64 / batch as f64 / 1e3;
                println!(
                    "  batch {batch:>2}: {:>8.1} us per call (p50), {:>8.1} us p99, \
                     {per_frame:>6.1} us per frame; batched against alone: max diff {worst:.2e}",
                    summary.p50 as f64 / 1e3,
                    summary.p99 as f64 / 1e3,
                );
                if i > 0 {
                    entry.push(',');
                }
                write!(
                    entry,
                    "{{\"batch\":{batch},\"call_us\":{},\"per_frame_us_p50\":{per_frame:.2},\
                     \"batched_vs_alone_max_diff\":{worst:e}}}",
                    summary.json(1e3)
                )?;
            }
            entry.push_str("]}");
            report.field(&entry);
        }
    }
    report.json.push(']');

    // --- End of turn: load, agree, cost ----------------------------------
    let mut front_end = MelFrontEnd::new();
    let mut features = vec![0.0_f32; FEATURES];
    let ends: Vec<usize> = (3..=35)
        .map(|half_seconds| half_seconds * SAMPLE_RATE / 2)
        .collect();
    let mut mel_times = Samples::with_capacity(ends.len());
    let mut windows = Vec::with_capacity(ends.len());
    for &end in &ends {
        let end = end.min(clip.len());
        let start = Instant::now();
        front_end.extract(&clip[..end], &mut features);
        mel_times.record_duration(start.elapsed());
        windows.push(features.clone());
    }
    let mel = mel_times.summary();
    println!(
        "\nlog-mel front end: {:.2} ms per 8 s window (p50), {:.2} ms max",
        mel.p50 as f64 / 1e6,
        mel.max as f64 / 1e6
    );
    report.field(&format!("\"mel_ms\":{}", mel.json(1e6)));

    report.field("\"turn\":[");
    let mut turn_reference: Option<Vec<f32>> = None;
    for kind in BACKENDS {
        let label = format!("{} / {TURN_MODEL}", kind.name());
        let start = Instant::now();
        let mut turn = match backend::load_turn(kind, &assets.join(TURN_MODEL)) {
            Ok(turn) => turn,
            Err(error) => {
                println!("\n{label}: DOES NOT LOAD: {error}");
                report.field(&format!(
                    "{{\"backend\":\"{}\",\"loads\":false}}",
                    kind.name()
                ));
                continue;
            }
        };
        let load_ms = start.elapsed().as_secs_f64() * 1e3;
        let mut probs = Vec::with_capacity(windows.len());
        let mut failed = None;
        for window in &windows {
            match turn.run(window) {
                Ok(prob) => probs.push(prob),
                Err(error) => {
                    failed = Some(error);
                    break;
                }
            }
        }
        if let Some(error) = failed {
            println!("\n{label}: loads but DOES NOT RUN: {error}");
            report.field(&format!(
                "{{\"backend\":\"{}\",\"loads\":true,\"runs\":false}}",
                kind.name()
            ));
            continue;
        }
        let mut times = Samples::with_capacity(200);
        for i in 0..220 {
            let start = Instant::now();
            turn.run(&windows[i % windows.len()])?;
            if i >= 20 {
                times.record_duration(start.elapsed());
            }
        }
        let summary = times.summary();
        let reference = turn_reference.get_or_insert_with(|| probs.clone());
        let (max_diff, mean_diff) = max_and_mean_diff(reference, &probs);
        println!(
            "\n{label}: loaded in {load_ms:.0} ms; {:.2} ms per inference (p50), {:.2} ms p99, \
             {:.2} ms max; against the reference: max diff {max_diff:.2e}, mean {mean_diff:.2e}",
            summary.p50 as f64 / 1e6,
            summary.p99 as f64 / 1e6,
            summary.max as f64 / 1e6,
        );
        let mut line = String::from("  complete-turn probability by clip position:");
        for (end, prob) in ends.iter().zip(&probs) {
            write!(line, " {:.1}s={prob:.2}", *end as f64 / SAMPLE_RATE as f64)?;
        }
        println!("{line}");
        let listed: Vec<String> = probs.iter().map(|p| format!("{p:.4}")).collect();
        report.field(&format!(
            "{{\"backend\":\"{}\",\"loads\":true,\"runs\":true,\"load_ms\":{load_ms:.1},\
             \"inference_ms\":{},\"max_diff_vs_reference\":{max_diff:e},\
             \"mean_diff_vs_reference\":{mean_diff:e},\"probabilities\":[{}]}}",
            kind.name(),
            summary.json(1e6),
            listed.join(",")
        ));
    }
    report.json.push(']');

    let calibration_after = calibrate();
    println!(
        "\ncalibration loop: {:.1} ms before, {:.1} ms after",
        calibration_before.as_secs_f64() * 1e3,
        calibration_after.as_secs_f64() * 1e3
    );
    report.field(&format!(
        "\"calibration_ms\":[{:.2},{:.2}]",
        calibration_before.as_secs_f64() * 1e3,
        calibration_after.as_secs_f64() * 1e3
    ));
    report.json.push('}');
    if let Some(out) = options.value("--out") {
        std::fs::write(Path::new(out), &report.json)?;
        println!("wrote {out}");
    }
    Ok(())
}
