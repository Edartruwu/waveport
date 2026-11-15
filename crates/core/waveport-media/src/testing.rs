//! Signals for this crate's tests.

use core::f64::consts::TAU;

use crate::rate::Rate;

/// `samples` samples of a sine wave of `hz` at half of full scale.
#[expect(
    clippy::cast_possible_truncation,
    reason = "half of full scale fits in a sample"
)]
#[expect(
    clippy::cast_precision_loss,
    reason = "sample counts in tests are small"
)]
pub(crate) fn tone(rate: Rate, hz: f64, samples: usize) -> Vec<i16> {
    let step = TAU * hz / f64::from(rate.hz());
    (0..samples)
        .map(|n| ((n as f64 * step).sin() * 16_384.0).round() as i16)
        .collect()
}

/// The energy of `signal`: the sum of its squared samples.
pub(crate) fn energy(signal: &[i16]) -> f64 {
    signal
        .iter()
        .map(|&sample| f64::from(sample) * f64::from(sample))
        .sum()
}

/// The share of the energy of `signal` that is a sine wave of `hz`, from 0
/// to 1. Silence has none.
#[expect(
    clippy::cast_precision_loss,
    reason = "sample counts in tests are small"
)]
pub(crate) fn share_at(rate: Rate, hz: f64, signal: &[i16]) -> f64 {
    let step = TAU * hz / f64::from(rate.hz());
    let (mut sine, mut cosine) = (0.0, 0.0);
    for (n, &sample) in signal.iter().enumerate() {
        let (s, c) = (n as f64 * step).sin_cos();
        sine += f64::from(sample) * s;
        cosine += f64::from(sample) * c;
    }
    let total = energy(signal);
    if total == 0.0 {
        return 0.0;
    }
    // A sine of amplitude a over n samples has energy a²n/2 and projects
    // to a·n/2 on its own shape.
    2.0 * (sine * sine + cosine * cosine) / (signal.len() as f64 * total)
}

#[cfg(test)]
mod tests {
    use super::{share_at, tone};
    use crate::rate::Rate;

    #[test]
    fn a_tone_is_all_at_its_own_frequency() {
        let rate = Rate::Hz16000;
        let signal = tone(rate, 440.0, 3_200);
        assert!(share_at(rate, 440.0, &signal) > 0.99);
        assert!(share_at(rate, 1_000.0, &signal) < 0.01);
        assert!(share_at(rate, 440.0, &[0; 320]).abs() < f64::EPSILON);
    }
}
