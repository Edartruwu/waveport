//! The Whisper log-mel front end that Smart Turn expects.
//!
//! Input: the last 8 s of audio at 16 kHz. Output: 80 mel bands by 800
//! frames. The steps follow the reference feature extractor: left-pad to
//! 8 s, normalise to zero mean and unit variance, reflect-pad by half a
//! window, 400-point Hann window with a 160-sample hop, power spectrum,
//! Slaney mel filters, log10, clamp to 8 below the peak, then `(x + 4) / 4`.
//!
//! The transform is a plain mixed-radix FFT (400 = 2^4 * 5^2). It is exact,
//! and within a small factor of a tuned one, which is what a cost model
//! needs.

pub const SAMPLE_RATE: usize = 16_000;
pub const N_FFT: usize = 400;
pub const HOP: usize = 160;
pub const N_MELS: usize = 80;
pub const FRAMES: usize = 800;
pub const SAMPLES: usize = HOP * FRAMES;
pub const FEATURES: usize = N_MELS * FRAMES;
const BINS: usize = N_FFT / 2 + 1;

#[derive(Clone, Copy, Default)]
struct Complex {
    re: f32,
    im: f32,
}

/// A triangular mel filter: its first bin and its non-zero weights.
struct Filter {
    first: usize,
    weights: Vec<f32>,
}

pub struct MelFrontEnd {
    window: Vec<f32>,
    roots: Vec<Complex>,
    filters: Vec<Filter>,
    padded: Vec<f32>,
    frame: Vec<f32>,
    spectrum: Vec<Complex>,
    power: Vec<f32>,
}

fn hz_to_mel(hz: f64) -> f64 {
    const F_SP: f64 = 200.0 / 3.0;
    const MIN_LOG_HZ: f64 = 1000.0;
    const MIN_LOG_MEL: f64 = MIN_LOG_HZ / F_SP;
    let log_step = 6.4_f64.ln() / 27.0;
    if hz >= MIN_LOG_HZ {
        MIN_LOG_MEL + (hz / MIN_LOG_HZ).ln() / log_step
    } else {
        hz / F_SP
    }
}

fn mel_to_hz(mel: f64) -> f64 {
    const F_SP: f64 = 200.0 / 3.0;
    const MIN_LOG_HZ: f64 = 1000.0;
    const MIN_LOG_MEL: f64 = MIN_LOG_HZ / F_SP;
    let log_step = 6.4_f64.ln() / 27.0;
    if mel >= MIN_LOG_MEL {
        MIN_LOG_HZ * (log_step * (mel - MIN_LOG_MEL)).exp()
    } else {
        mel * F_SP
    }
}

fn mel_filters() -> Vec<Filter> {
    let nyquist = SAMPLE_RATE as f64 / 2.0;
    let top = hz_to_mel(nyquist);
    let edges: Vec<f64> = (0..N_MELS + 2)
        .map(|i| mel_to_hz(top * i as f64 / (N_MELS + 1) as f64))
        .collect();
    let bin_hz = |bin: usize| nyquist * bin as f64 / (BINS - 1) as f64;
    (0..N_MELS)
        .map(|m| {
            let (low, mid, high) = (edges[m], edges[m + 1], edges[m + 2]);
            let norm = 2.0 / (high - low);
            let weight = |bin: usize| {
                let hz = bin_hz(bin);
                let rising = (hz - low) / (mid - low);
                let falling = (high - hz) / (high - mid);
                rising.min(falling).max(0.0) * norm
            };
            let first = (0..BINS).find(|&bin| weight(bin) > 0.0).unwrap_or(0);
            let weights = (first..BINS)
                .map(weight)
                .take_while(|&w| w > 0.0)
                .map(|w| w as f32)
                .collect();
            Filter { first, weights }
        })
        .collect()
}

/// The discrete Fourier transform of `n` real samples taken from `input`
/// starting at `offset` with step `stride`. `roots` holds the N-th roots of
/// unity for some multiple N of `n`.
fn fft(
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
    let radix = if n % 2 == 0 { 2 } else { 5 };
    let part = n / radix;
    for r in 0..radix {
        fft(
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
        for r in 0..radix {
            terms[r] = out[r * part + k];
        }
        for q in 0..radix {
            let index = k + q * part;
            let mut sum = terms[0];
            for r in 1..radix {
                let w = roots[(r * index * step) % roots.len()];
                sum.re += terms[r].re * w.re - terms[r].im * w.im;
                sum.im += terms[r].re * w.im + terms[r].im * w.re;
            }
            out[index] = sum;
        }
    }
}

impl Default for MelFrontEnd {
    fn default() -> Self {
        Self::new()
    }
}

impl MelFrontEnd {
    #[must_use]
    pub fn new() -> Self {
        let tau = std::f64::consts::TAU;
        Self {
            // The periodic Hann window.
            window: (0..N_FFT)
                .map(|i| (0.5 - 0.5 * (tau * i as f64 / N_FFT as f64).cos()) as f32)
                .collect(),
            roots: (0..N_FFT)
                .map(|k| {
                    let angle = -tau * k as f64 / N_FFT as f64;
                    Complex {
                        re: angle.cos() as f32,
                        im: angle.sin() as f32,
                    }
                })
                .collect(),
            filters: mel_filters(),
            padded: vec![0.0; SAMPLES + N_FFT],
            frame: vec![0.0; N_FFT],
            spectrum: vec![Complex::default(); N_FFT],
            power: vec![0.0; BINS],
        }
    }

    /// Computes the features of the end of `audio` into `out`, which holds
    /// `N_MELS * FRAMES` values laid out band by band.
    pub fn extract(&mut self, audio: &[f32], out: &mut [f32]) {
        assert_eq!(out.len(), FEATURES);
        let half = N_FFT / 2;
        let tail = &audio[audio.len().saturating_sub(SAMPLES)..];
        let lead = SAMPLES - tail.len();

        // Left-pad with silence, then normalise over the whole window.
        let body = &mut self.padded[half..half + SAMPLES];
        body[..lead].fill(0.0);
        body[lead..].copy_from_slice(tail);
        let mean = body.iter().map(|&x| f64::from(x)).sum::<f64>() / SAMPLES as f64;
        let variance = body
            .iter()
            .map(|&x| (f64::from(x) - mean).powi(2))
            .sum::<f64>()
            / SAMPLES as f64;
        let scale = 1.0 / (variance + 1e-7).sqrt();
        for x in body.iter_mut() {
            *x = ((f64::from(*x) - mean) * scale) as f32;
        }

        // Reflect half a window at each end, so that frames are centred.
        for i in 0..half {
            self.padded[i] = self.padded[2 * half - i];
            self.padded[half + SAMPLES + i] = self.padded[half + SAMPLES - 2 - i];
        }

        let mut peak = f32::MIN;
        for t in 0..FRAMES {
            let start = t * HOP;
            for (i, slot) in self.frame.iter_mut().enumerate() {
                *slot = self.padded[start + i] * self.window[i];
            }
            fft(&self.frame, 0, 1, N_FFT, &mut self.spectrum, &self.roots);
            for (bin, slot) in self.power.iter_mut().enumerate() {
                let c = self.spectrum[bin];
                *slot = c.re * c.re + c.im * c.im;
            }
            for (m, filter) in self.filters.iter().enumerate() {
                let energy: f32 = filter
                    .weights
                    .iter()
                    .zip(&self.power[filter.first..])
                    .map(|(w, p)| w * p)
                    .sum();
                let log = energy.max(1e-10).log10();
                peak = peak.max(log);
                out[m * FRAMES + t] = log;
            }
        }
        let floor = peak - 8.0;
        for value in out.iter_mut() {
            *value = (value.max(floor) + 4.0) / 4.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Complex, N_FFT, fft};

    #[test]
    fn the_transform_matches_the_definition() {
        let tau = std::f64::consts::TAU;
        let roots: Vec<Complex> = (0..N_FFT)
            .map(|k| {
                let angle = -tau * k as f64 / N_FFT as f64;
                Complex {
                    re: angle.cos() as f32,
                    im: angle.sin() as f32,
                }
            })
            .collect();
        let input: Vec<f32> = (0..N_FFT)
            .map(|i| ((i * 37 % 101) as f32 / 50.0) - 1.0)
            .collect();
        let mut out = vec![Complex::default(); N_FFT];
        fft(&input, 0, 1, N_FFT, &mut out, &roots);
        for bin in [0, 1, 7, 100, 200, 399] {
            let (mut re, mut im) = (0.0_f64, 0.0_f64);
            for (i, &x) in input.iter().enumerate() {
                let angle = -tau * (bin * i) as f64 / N_FFT as f64;
                re += f64::from(x) * angle.cos();
                im += f64::from(x) * angle.sin();
            }
            assert!((f64::from(out[bin].re) - re).abs() < 1e-2, "bin {bin} re");
            assert!((f64::from(out[bin].im) - im).abs() < 1e-2, "bin {bin} im");
        }
    }
}
