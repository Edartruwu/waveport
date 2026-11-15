//! What the end-of-turn model is given to look at: the sound of the last
//! eight seconds, as 80 bands of pitch by 800 steps of time.
//!
//! The model was trained on the features a Whisper model is given, so
//! the steps here are those of that model's feature extractor, in its
//! order: silence is put before audio that is shorter than eight
//! seconds; the whole is brought to a mean of nought and a variance of
//! one; half a window is mirrored on to each end; each step of 10 ms is
//! a window of 25 ms, weighted and transformed; the power in each
//! frequency is gathered into bands that are evenly spaced as pitch is
//! heard; and the logarithm of each band is kept, no lower than eight
//! below the loudest, and scaled.
//!
//! The transform is a plain one, exact, for windows of 400 samples
//! (2⁴ × 5²). A window of features takes some ten milliseconds, against
//! forty or so for the model itself.

use std::f64::consts::TAU;

/// Samples to a second of the audio.
const RATE: usize = 16_000;
/// Samples in a window.
const WINDOW: usize = 400;
/// Samples from one window to the next.
const HOP: usize = 160;
/// Bands.
pub(crate) const BANDS: usize = 80;
/// Steps of time.
pub(crate) const STEPS: usize = 800;
/// Samples of audio that are looked at: eight seconds.
pub(crate) const SAMPLES: usize = HOP * STEPS;
/// Values in the features of one stretch of audio.
pub(crate) const FEATURES: usize = BANDS * STEPS;
/// Frequencies a window is told apart into.
const BINS: usize = WINDOW / 2 + 1;

#[derive(Clone, Copy, Default)]
struct Complex {
    re: f32,
    im: f32,
}

/// One band: the first frequency it gathers, and how much of that one
/// and of each that follows.
struct Band {
    first: usize,
    weights: Vec<f32>,
}

/// Where a frequency is on the scale of pitch as it is heard: even steps
/// up to 1,000 Hz, and steps that grow with the frequency above.
fn pitch_of(hz: f64) -> f64 {
    const STEP: f64 = 200.0 / 3.0;
    const BEND: f64 = 1_000.0;
    if hz >= BEND {
        BEND / STEP + (hz / BEND).ln() / (6.4_f64.ln() / 27.0)
    } else {
        hz / STEP
    }
}

/// The frequency at a place on the scale of pitch.
fn hz_of(pitch: f64) -> f64 {
    const STEP: f64 = 200.0 / 3.0;
    const BEND: f64 = 1_000.0;
    if pitch >= BEND / STEP {
        BEND * ((6.4_f64.ln() / 27.0) * (pitch - BEND / STEP)).exp()
    } else {
        pitch * STEP
    }
}

#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "counts of a few hundred, and weights that are kept as f32"
)]
fn bands() -> Vec<Band> {
    let top_hz = RATE as f64 / 2.0;
    let top = pitch_of(top_hz);
    // The bands are triangles that overlap by half: each begins where
    // the one before it peaks.
    let edge = |n: usize| hz_of(top * n as f64 / (BANDS + 1) as f64);
    (0..BANDS)
        .map(|band| {
            let (low, peak, high) = (edge(band), edge(band + 1), edge(band + 2));
            let area = 2.0 / (high - low);
            let weight = |bin: usize| {
                let hz = top_hz * bin as f64 / (BINS - 1) as f64;
                let rising = (hz - low) / (peak - low);
                let falling = (high - hz) / (high - peak);
                rising.min(falling).max(0.0) * area
            };
            let first = (0..BINS).find(|bin| weight(*bin) > 0.0).unwrap_or(0);
            let weights = (first..BINS)
                .map(weight)
                .take_while(|weight| *weight > 0.0)
                .map(|weight| weight as f32)
                .collect();
            Band { first, weights }
        })
        .collect()
}

/// The transform of `n` samples of `input`, every `stride`-th from
/// `offset` on, into `out`, which is `n` long. `roots` are the roots of
/// one, as many as a multiple of `n`.
#[expect(
    clippy::indexing_slicing,
    reason = "every place is within lengths that are fixed where the function is called, \
              and a test holds the transform to its definition"
)]
fn transform(
    input: &[f32],
    offset: usize,
    stride: usize,
    n: usize,
    out: &mut [Complex],
    roots: &[Complex],
) {
    if n == 1 {
        out[0] = Complex {
            re: input[offset],
            im: 0.0,
        };
        return;
    }
    let radix = if n.is_multiple_of(2) { 2 } else { 5 };
    let part = n / radix;
    for r in 0..radix {
        transform(
            input,
            offset + r * stride,
            stride * radix,
            part,
            &mut out[r * part..(r + 1) * part],
            roots,
        );
    }
    let step = roots.len() / n;
    for k in 0..part {
        let mut terms = [Complex::default(); 5];
        for (r, term) in terms.iter_mut().enumerate().take(radix) {
            *term = out[r * part + k];
        }
        for q in 0..radix {
            let index = k + q * part;
            let mut sum = terms[0];
            for (r, term) in terms.iter().enumerate().take(radix).skip(1) {
                let root = roots[(r * index * step) % roots.len()];
                sum.re += term.re * root.re - term.im * root.im;
                sum.im += term.re * root.im + term.im * root.re;
            }
            out[index] = sum;
        }
    }
}

/// Makes the features of audio. It holds the tables and the room to work
/// in, so that making features allocates nothing.
pub(crate) struct Mel {
    /// The weight of each sample of a window.
    weights: Vec<f32>,
    roots: Vec<Complex>,
    bands: Vec<Band>,
    /// The audio, with half a window mirrored at each end.
    padded: Vec<f32>,
    window: Vec<f32>,
    spectrum: Vec<Complex>,
    power: Vec<f32>,
}

impl Mel {
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        reason = "counts of a few hundred, and tables that are kept as f32"
    )]
    pub(crate) fn new() -> Self {
        let turn = |k: usize| TAU * k as f64 / WINDOW as f64;
        Self {
            weights: (0..WINDOW)
                .map(|n| (0.5 - 0.5 * turn(n).cos()) as f32)
                .collect(),
            roots: (0..WINDOW)
                .map(|k| Complex {
                    re: turn(k).cos() as f32,
                    im: -turn(k).sin() as f32,
                })
                .collect(),
            bands: bands(),
            padded: vec![0.0; SAMPLES + WINDOW],
            window: vec![0.0; WINDOW],
            spectrum: vec![Complex::default(); WINDOW],
            power: vec![0.0; BINS],
        }
    }

    /// Makes the features of the end of `audio`: of its last eight
    /// seconds, or of all of it after as much silence as makes eight.
    /// `out` is [`FEATURES`] long, band after band.
    #[expect(
        clippy::indexing_slicing,
        reason = "every place is within lengths that are fixed by the constants above"
    )]
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        reason = "a count of samples, and a sample that is kept as f32"
    )]
    pub(crate) fn features(&mut self, audio: &[i16], out: &mut [f32]) {
        debug_assert_eq!(out.len(), FEATURES, "room for the features");
        let half = WINDOW / 2;
        let tail = &audio[audio.len().saturating_sub(SAMPLES)..];
        let lead = SAMPLES - tail.len();

        // Silence first, then the audio, and the whole to a mean of
        // nought and a variance of one.
        let body = &mut self.padded[half..half + SAMPLES];
        body[..lead].fill(0.0);
        for (slot, sample) in body[lead..].iter_mut().zip(tail) {
            *slot = f32::from(*sample) / 32_768.0;
        }
        let mean = body.iter().map(|x| f64::from(*x)).sum::<f64>() / SAMPLES as f64;
        let variance = body
            .iter()
            .map(|x| (f64::from(*x) - mean).powi(2))
            .sum::<f64>()
            / SAMPLES as f64;
        let scale = 1.0 / (variance + 1e-7).sqrt();
        for x in body.iter_mut() {
            *x = ((f64::from(*x) - mean) * scale) as f32;
        }

        // Half a window mirrored at each end, so that a step is the
        // middle of its window.
        for n in 0..half {
            self.padded[n] = self.padded[2 * half - n];
            self.padded[half + SAMPLES + n] = self.padded[half + SAMPLES - 2 - n];
        }

        let mut loudest = f32::MIN;
        for step in 0..STEPS {
            let from = step * HOP;
            for (n, slot) in self.window.iter_mut().enumerate() {
                *slot = self.padded[from + n] * self.weights[n];
            }
            transform(&self.window, 0, 1, WINDOW, &mut self.spectrum, &self.roots);
            for (slot, c) in self.power.iter_mut().zip(&self.spectrum) {
                *slot = c.re * c.re + c.im * c.im;
            }
            for (band, gathered) in self.bands.iter().enumerate() {
                let energy: f32 = gathered
                    .weights
                    .iter()
                    .zip(&self.power[gathered.first..])
                    .map(|(weight, power)| weight * power)
                    .sum();
                let log = energy.max(1e-10).log10();
                loudest = loudest.max(log);
                out[band * STEPS + step] = log;
            }
        }
        let floor = loudest - 8.0;
        for value in out.iter_mut() {
            *value = (value.max(floor) + 4.0) / 4.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::f64::consts::TAU;

    use super::{BANDS, Complex, FEATURES, Mel, SAMPLES, STEPS, WINDOW, bands, transform};

    #[test]
    fn the_transform_is_the_one_its_definition_gives() {
        let mel = Mel::new();
        let input: Vec<f32> = (0..WINDOW)
            .map(|n| f32::from(u8::try_from(n * 37 % 101).unwrap()) / 50.0 - 1.0)
            .collect();
        let mut out = vec![Complex::default(); WINDOW];
        transform(&input, 0, 1, WINDOW, &mut out, &mel.roots);
        for bin in [0_usize, 1, 7, 100, 200, 399] {
            let (mut re, mut im) = (0.0_f64, 0.0_f64);
            for (n, x) in input.iter().enumerate() {
                let angle = -TAU * f64::from(u32::try_from(bin * n).unwrap()) / 400.0;
                re += f64::from(*x) * angle.cos();
                im += f64::from(*x) * angle.sin();
            }
            assert!((f64::from(out[bin].re) - re).abs() < 1e-2, "bin {bin}");
            assert!((f64::from(out[bin].im) - im).abs() < 1e-2, "bin {bin}");
        }
    }

    #[test]
    fn the_bands_cover_the_frequencies_from_low_to_high() {
        let bands = bands();
        assert_eq!(bands.len(), BANDS);
        // Each begins no lower than the one before it, and gathers
        // something.
        let mut before = 0;
        for band in &bands {
            assert!(band.first >= before);
            assert!(band.weights.first().is_some_and(|weight| *weight > 0.0));
            assert!(band.weights.iter().all(|weight| *weight > 0.0));
            before = band.first;
        }
        // The lowest gathers the lowest frequencies, and the highest
        // reaches the top.
        assert_eq!(bands[0].first, 1);
        let top = &bands[BANDS - 1];
        assert_eq!(top.first + top.weights.len(), 200);
    }

    /// A tone of `hz` for `samples` samples.
    fn tone(hz: f64, samples: usize) -> Vec<i16> {
        (0..samples)
            .map(|n| {
                let turn = TAU * hz * f64::from(u32::try_from(n).unwrap()) / 16_000.0;
                #[expect(clippy::cast_possible_truncation, reason = "within 8,000 either way")]
                let sample = (turn.sin() * 8_000.0) as i16;
                sample
            })
            .collect()
    }

    /// The band that is loudest at `step`.
    fn loudest_band(features: &[f32], step: usize) -> usize {
        (0..BANDS)
            .max_by(|a, b| features[a * STEPS + step].total_cmp(&features[b * STEPS + step]))
            .unwrap()
    }

    #[test]
    fn a_tone_is_loudest_in_its_band_and_a_higher_tone_in_a_higher_one() {
        let mut mel = Mel::new();
        let mut low = vec![0.0; FEATURES];
        let mut high = vec![0.0; FEATURES];
        mel.features(&tone(300.0, SAMPLES), &mut low);
        mel.features(&tone(3_000.0, SAMPLES), &mut high);
        let (low_band, high_band) = (loudest_band(&low, 400), loudest_band(&high, 400));
        assert!(low_band < 12, "{low_band}");
        assert!((40..60).contains(&high_band), "{high_band}");
        // Scaled as the model expects: the loudest value of all is
        // near one and a half, and nothing is more than two below it.
        let peak = low.iter().copied().fold(f32::MIN, f32::max);
        let least = low.iter().copied().fold(f32::MAX, f32::min);
        assert!((0.5..2.5).contains(&peak), "{peak}");
        assert!((peak - least - 2.0).abs() < 1e-4, "{peak} {least}");
    }

    #[test]
    fn audio_that_is_short_comes_after_silence_and_long_audio_gives_its_end() {
        let mut mel = Mel::new();
        // Two seconds of a tone: the first six seconds of the features
        // are the silence put before it.
        let mut short = vec![0.0; FEATURES];
        mel.features(&tone(1_000.0, 32_000), &mut short);
        let band = loudest_band(&short, 700);
        let early = short[band * STEPS + 100];
        let late = short[band * STEPS + 700];
        assert!(late - early > 1.0, "{early} {late}");

        // Twelve seconds, of which the last eight are a higher tone:
        // only those are looked at.
        let mut long = tone(300.0, 64_000);
        long.extend(tone(3_000.0, SAMPLES));
        let mut end = vec![0.0; FEATURES];
        mel.features(&long, &mut end);
        let mut alone = vec![0.0; FEATURES];
        mel.features(&tone(3_000.0, SAMPLES), &mut alone);
        assert_eq!(end, alone);

        // Nothing at all is silence, and makes features without a fuss.
        let mut nothing = vec![0.0; FEATURES];
        mel.features(&[], &mut nothing);
        assert!(nothing.iter().all(|value| value.is_finite()));
    }
}
