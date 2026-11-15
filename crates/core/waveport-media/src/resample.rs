//! Changing the sample rate.
//!
//! The media path works at four rates, so there are twelve conversions,
//! each by a ratio of two small whole numbers. A [`Resampler`] does one of
//! them, on a stream given to it in pieces of any length.
//!
//! The method is the textbook one: think of `up - 1` zeros put after every
//! sample, a low-pass filter run over the result, and every `down`-th
//! sample of that kept. The zeros are never stored: for each sample that
//! is kept only the filter coefficients that meet real samples are used,
//! and there are `up` such sets of coefficients, one for each place a kept
//! sample can fall between two samples of the input.
//!
//! The filter passes 85% of the band that both rates can hold, and
//! reduces by 60 dB what the lower rate cannot hold. Its coefficients are
//! whole numbers worked out with arithmetic alone, so the same input gives
//! the same output on every platform.

use core::f64::consts::PI;
use core::fmt;

use waveport_domain::time::MediaDur;

use crate::rate::Rate;

/// Filter coefficients for each output sample, counted in samples of the
/// lower of the two rates.
const BASE_TAPS: usize = 48;

/// The shape of the Kaiser window that gives 60 dB.
const KAISER_BETA: f64 = 5.653;

/// The width of the band between what the filter passes and what it
/// stops, in units of the sample rate over the filter length.
const TRANSITION: f64 = 3.625;

/// One, as a coefficient.
///
/// A coefficient has 14 bits after the point and a sample has 16 bits, so
/// that the products of one set always add up within 32 bits, which the
/// compiler can then do eight at a time. A test holds the sets to that.
const ONE: i32 = 1 << 14;

/// The longest piece of input worked on at once.
const PIECE: usize = 960;

/// `sin(πx)`, from arithmetic alone.
fn sin_pi(x: f64) -> f64 {
    // sin(πx) is ±sin(πr) for the r in [-0.5, 0.5) that differs from x by
    // a whole number.
    let whole = (x + 0.5).floor();
    let r = x - whole;
    let sign = if whole % 2.0 == 0.0 { 1.0 } else { -1.0 };
    let t = PI * r;
    let t2 = t * t;
    let (mut term, mut sum) = (t, t);
    // At π/2 the thirteenth term is below 1e-20.
    for n in 1..=12 {
        let n = f64::from(n);
        term *= -t2 / ((2.0 * n) * (2.0 * n + 1.0));
        sum += term;
    }
    sign * sum
}

/// The modified Bessel function of the first kind and order zero.
fn bessel_i0(x: f64) -> f64 {
    let quarter = x * x / 4.0;
    let (mut term, mut sum) = (1.0, 1.0);
    for k in 1..=40 {
        let k = f64::from(k);
        term *= quarter / (k * k);
        sum += term;
    }
    sum
}

/// The coefficients of the low-pass filter for a conversion, one set for
/// each of the `up` places an output sample can fall. Each set is in the
/// order of the samples it multiplies, oldest first, and adds up to
/// exactly one.
#[expect(
    clippy::cast_possible_truncation,
    reason = "a coefficient is at most one, which is 16,384"
)]
#[expect(
    clippy::cast_precision_loss,
    reason = "filter lengths are a few thousand at most"
)]
fn design(from: Rate, to: Rate, up: usize, taps: usize) -> Box<[i16]> {
    let length = up * taps;
    let lower = f64::from(from.hz().min(to.hz()));
    // The rate of the stream with the zeros in it.
    let dense = f64::from(from.hz()) * up as f64;
    let transition = TRANSITION * dense / length as f64;
    // The filter is half way down in the middle of the transition band,
    // and that band ends where the lower rate's band does.
    let cutoff = (lower / 2.0 - transition / 2.0) / dense;
    let centre = (length as f64 - 1.0) / 2.0;
    let window_scale = bessel_i0(KAISER_BETA);

    let prototype = |i: usize| -> f64 {
        let at = i as f64 - centre;
        let ideal = if at == 0.0 {
            2.0 * cutoff
        } else {
            sin_pi(2.0 * cutoff * at) / (PI * at)
        };
        let across = at / centre;
        let window = bessel_i0(KAISER_BETA * (1.0 - across * across).max(0.0).sqrt());
        // The zeros take all but one part in `up` of the level away.
        up as f64 * ideal * window / window_scale
    };

    let mut coefficients = vec![0_i16; length];
    for (phase, set) in coefficients.chunks_exact_mut(taps).enumerate() {
        let mut sum = 0;
        let mut largest = 0;
        let mut largest_at = 0;
        for (j, coefficient) in set.iter_mut().enumerate() {
            let value = (prototype(phase + (taps - 1 - j) * up) * f64::from(ONE)).round() as i32;
            let value = value.clamp(i32::from(i16::MIN), i32::from(i16::MAX));
            *coefficient = value as i16;
            sum += value;
            if value.abs() > largest {
                largest = value.abs();
                largest_at = j;
            }
        }
        // Rounding leaves each set a little off one. A steady input must
        // come out steady, so the difference goes to the largest.
        if let Some(coefficient) = set.get_mut(largest_at) {
            let fixed = i32::from(*coefficient) + (ONE - sum);
            *coefficient = fixed.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        }
    }
    coefficients.into_boxed_slice()
}

/// There is not enough room for what a call would write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoRoom {
    /// How many samples the call needs room for.
    pub needed: usize,
}

impl fmt::Display for NoRoom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the output needs room for {} samples", self.needed)
    }
}

impl core::error::Error for NoRoom {}

/// Converts one stream of audio from one sample rate to another.
#[derive(Clone)]
pub struct Resampler {
    from: Rate,
    to: Rate,
    /// Output samples for every `down` input samples.
    up: usize,
    down: usize,
    /// Coefficients in each set.
    taps: usize,
    coefficients: Box<[i16]>,
    /// The last `taps - 1` samples of the input before the piece being
    /// worked on, then that piece.
    line: Box<[i16]>,
    /// Where the next output sample falls, counted from the start of the
    /// next piece of input in steps of one `up`-th of an input sample.
    position: usize,
}

impl Resampler {
    /// A converter from `from` to `to`. With both the same it passes
    /// audio through unchanged.
    #[must_use]
    pub fn new(from: Rate, to: Rate) -> Self {
        // Every rate is a multiple of 8 kHz, by 1, 2, 3 or 6.
        let unit = |rate: Rate| (rate.hz() / 8_000) as usize;
        let (mut up, mut down) = (unit(to), unit(from));
        for shared in [2, 3] {
            while up % shared == 0 && down % shared == 0 {
                up /= shared;
                down /= shared;
            }
        }
        if from == to {
            return Self {
                from,
                to,
                up: 1,
                down: 1,
                taps: 1,
                coefficients: Box::default(),
                line: Box::default(),
                position: 0,
            };
        }
        // A filter that has to cut lower is longer by as much.
        let taps = if down > up {
            BASE_TAPS * down / up
        } else {
            BASE_TAPS
        };
        Self {
            from,
            to,
            up,
            down,
            taps,
            coefficients: design(from, to, up, taps),
            line: vec![0; taps - 1 + PIECE].into_boxed_slice(),
            position: 0,
        }
    }

    /// The rate of the audio it takes.
    #[must_use]
    pub const fn from(&self) -> Rate {
        self.from
    }

    /// The rate of the audio it gives.
    #[must_use]
    pub const fn to(&self) -> Rate {
        self.to
    }

    /// How far behind its input the output is: the filter gives each
    /// sample only once it has seen what follows it.
    #[must_use]
    pub fn delay(&self) -> MediaDur {
        if self.coefficients.is_empty() {
            return MediaDur::ZERO;
        }
        // Half the filter's length, which is counted in steps of one
        // `up`-th of an input sample. Rounded to the nearest tick.
        let half_steps = self.up * self.taps - 1;
        let ticks = self.from.ticks_per_sample() as usize;
        let ticks = (half_steps * ticks + self.up) / (2 * self.up);
        MediaDur::from_ticks(u32::try_from(ticks).unwrap_or(u32::MAX))
    }

    /// How many samples the next `input` samples of input will give.
    #[must_use]
    pub const fn yields(&self, input: usize) -> usize {
        let end = input * self.up;
        if end > self.position {
            (end - self.position).div_ceil(self.down)
        } else {
            0
        }
    }

    /// Forgets the stream so far: the next input is the start of another.
    pub fn reset(&mut self) {
        self.line.fill(0);
        self.position = 0;
    }

    /// Converts `input`, the next piece of the stream, into the start of
    /// `out`, and returns how many samples it wrote:
    /// [`yields(input.len())`](Self::yields) of them.
    ///
    /// # Errors
    ///
    /// `out` has less room than that. Nothing is taken then.
    pub fn process(&mut self, input: &[i16], out: &mut [i16]) -> Result<usize, NoRoom> {
        let needed = self.yields(input.len());
        let Some(mut out) = out.get_mut(..needed) else {
            return Err(NoRoom { needed });
        };
        if self.coefficients.is_empty() {
            out.copy_from_slice(input);
            return Ok(needed);
        }
        for piece in input.chunks(PIECE) {
            let written = self.piece(piece, out);
            out = out.get_mut(written..).unwrap_or(&mut []);
        }
        Ok(needed)
    }

    /// Converts a piece of at most [`PIECE`] samples into the start of
    /// `out`, which has room, and returns how many samples it wrote.
    fn piece(&mut self, piece: &[i16], out: &mut [i16]) -> usize {
        let history = self.taps - 1;
        if let Some(room) = self.line.get_mut(history..history + piece.len()) {
            room.copy_from_slice(piece);
        }
        let end = piece.len() * self.up;
        let mut written = 0;
        for sample in out.iter_mut() {
            if self.position >= end {
                break;
            }
            // The newest input sample this output depends on, and which
            // set of coefficients its place between two samples calls for.
            let newest = self.position / self.up;
            let phase = self.position % self.up;
            let window = self.line.get(newest..newest + self.taps);
            let set = self
                .coefficients
                .get(phase * self.taps..(phase + 1) * self.taps);
            let (Some(window), Some(set)) = (window, set) else {
                break;
            };
            let sum: i32 = window
                .iter()
                .zip(set)
                .map(|(&x, &c)| i32::from(x) * i32::from(c))
                .sum();
            let rounded = (sum + ONE / 2) >> 14;
            *sample =
                i16::try_from(rounded).unwrap_or(if rounded < 0 { i16::MIN } else { i16::MAX });
            self.position += self.down;
            written += 1;
        }
        self.position -= end;
        // The end of this piece is what the next one looks back on.
        self.line.copy_within(piece.len()..piece.len() + history, 0);
        written
    }
}

impl fmt::Debug for Resampler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resampler")
            .field("from", &self.from)
            .field("to", &self.to)
            .field("taps", &self.taps)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use core::f64::consts::PI;

    use super::{NoRoom, ONE, Resampler, bessel_i0, sin_pi};
    use crate::rate::Rate;
    use crate::testing::{energy, share_at, tone};

    /// Every conversion between two different rates.
    fn conversions() -> impl Iterator<Item = (Rate, Rate)> {
        Rate::ALL
            .into_iter()
            .flat_map(|from| Rate::ALL.into_iter().map(move |to| (from, to)))
            .filter(|(from, to)| from != to)
    }

    /// The energy of `signal` for each sample of it.
    #[expect(clippy::cast_precision_loss, reason = "test signals are short")]
    fn level(signal: &[i16]) -> f64 {
        energy(signal) / signal.len() as f64
    }

    /// Converts one second of a tone and returns what comes out once the
    /// filter has filled.
    fn converted(from: Rate, to: Rate, hz: f64) -> Vec<i16> {
        let mut resampler = Resampler::new(from, to);
        let input = tone(from, hz, from.hz() as usize);
        let mut out = vec![0; resampler.yields(input.len())];
        let written = resampler.process(&input, &mut out).unwrap();
        assert_eq!(written, to.hz() as usize);
        out.split_off(to.hz() as usize / 10)
    }

    #[test]
    fn the_arithmetic_stands_in_for_the_library() {
        for i in -400..=400 {
            let x = f64::from(i) / 37.0;
            assert!((sin_pi(x) - (PI * x).sin()).abs() < 1e-13, "{x}");
        }
        assert!(sin_pi(3.0).abs() < 1e-15);
        // Values from tables.
        assert!((bessel_i0(0.0) - 1.0).abs() < 1e-15);
        assert!((bessel_i0(1.0) - 1.266_065_877_752_008).abs() < 1e-14);
        assert!((bessel_i0(5.0) - 27.239_871_823_604_44).abs() < 1e-12);
    }

    #[test]
    fn each_set_of_coefficients_adds_up_to_one() {
        for (from, to) in conversions() {
            let resampler = Resampler::new(from, to);
            for set in resampler.coefficients.chunks(resampler.taps) {
                let sum: i32 = set.iter().map(|&c| i32::from(c)).sum();
                assert_eq!(sum, ONE, "{from:?} to {to:?}");
            }
        }
    }

    #[test]
    fn the_loudest_input_adds_up_within_32_bits() {
        for (from, to) in conversions() {
            let resampler = Resampler::new(from, to);
            for set in resampler.coefficients.chunks(resampler.taps) {
                // The most a set can add up to: every sample at the far
                // end of its range, with the sign of its coefficient.
                let reach: i64 = set.iter().map(|&c| i64::from(c).abs()).sum();
                let most = reach * 32_768 + i64::from(ONE / 2);
                assert!(most <= i64::from(i32::MAX), "{from:?} to {to:?}: {most}");
            }
        }
    }

    #[test]
    fn a_steady_level_comes_out_as_it_went_in() {
        for (from, to) in conversions() {
            for level in [1_000, -1, i16::MAX, i16::MIN] {
                let mut resampler = Resampler::new(from, to);
                let input = vec![level; from.frame() * 10];
                let mut out = vec![0; to.frame() * 10];
                resampler.process(&input, &mut out).unwrap();
                let settled = &out[to.frame() * 5..];
                assert!(
                    settled.iter().all(|&sample| sample == level),
                    "{from:?} to {to:?} at {level}"
                );
            }
        }
    }

    #[test]
    fn what_both_rates_can_hold_passes_unchanged() {
        for (from, to) in conversions() {
            let lower = f64::from(from.hz().min(to.hz()));
            // From low in the band up to 84% of it.
            for share in [0.02, 0.2, 0.42] {
                let hz = lower * share;
                let out = converted(from, to, hz);
                let change = level(&out) / level(&tone(from, hz, 4_000));
                assert!(
                    (0.97..=1.03).contains(&change),
                    "{from:?} to {to:?} at {hz} Hz: level changed by {change}"
                );
                assert!(
                    share_at(to, hz, &out) > 0.999,
                    "{from:?} to {to:?} at {hz} Hz"
                );
            }
        }
    }

    #[test]
    fn what_the_lower_rate_cannot_hold_is_removed() {
        for (from, to) in conversions() {
            let lower = f64::from(from.hz().min(to.hz()));
            // Just past the band, and well past it. Going down, such a tone
            // would come out as a false one. Going up, the false one is the
            // echo of a true tone above the old band.
            for share in [0.52, 0.6, 0.9] {
                let (hz, false_hz) = if from > to {
                    (lower * share, lower * share)
                } else {
                    (lower * (1.0 - share), lower * share)
                };
                if hz >= f64::from(from.hz()) / 2.0 || hz <= 0.0 {
                    continue;
                }
                let out = converted(from, to, hz);
                let left = if from > to {
                    level(&out)
                } else {
                    level(&out) * share_at(to, false_hz, &out)
                };
                let reduction = left / level(&tone(from, hz, 4_000));
                // 55 dB.
                assert!(
                    reduction < 3.2e-6,
                    "{from:?} to {to:?} at {hz} Hz: {reduction:e} is left"
                );
            }
        }
    }

    #[test]
    fn the_pieces_a_stream_comes_in_do_not_matter() {
        for (from, to) in conversions() {
            let input = tone(from, 523.0, from.frame() * 12 + 7);
            let mut whole = Resampler::new(from, to);
            let mut expected = vec![0; whole.yields(input.len())];
            whole.process(&input, &mut expected).unwrap();

            let mut pieces = Resampler::new(from, to);
            let mut got = Vec::new();
            let mut rest = input.as_slice();
            let mut size = 1;
            while !rest.is_empty() {
                let (piece, after) = rest.split_at(size.min(rest.len()));
                let mut out = vec![0; pieces.yields(piece.len())];
                let written = pieces.process(piece, &mut out).unwrap();
                assert_eq!(written, out.len());
                got.extend(out);
                rest = after;
                // 1, 4, 13, 40, ... and back to small ones.
                size = (size * 3 + 1) % 1_500;
            }
            assert_eq!(got, expected, "{from:?} to {to:?}");
        }
    }

    #[test]
    fn a_frame_in_is_a_frame_out() {
        for (from, to) in conversions() {
            let mut resampler = Resampler::new(from, to);
            let frame = vec![0; from.frame()];
            for _ in 0..5 {
                assert_eq!(resampler.yields(from.frame()), to.frame());
                let mut out = vec![0; to.frame()];
                assert_eq!(resampler.process(&frame, &mut out), Ok(to.frame()));
            }
            // Too little room is noticed before anything is taken.
            let mut short = vec![0; to.frame() - 1];
            assert_eq!(
                resampler.process(&frame, &mut short),
                Err(NoRoom { needed: to.frame() })
            );
            assert_eq!(resampler.yields(from.frame()), to.frame());
            assert_eq!(resampler.process(&[], &mut []), Ok(0));
        }
    }

    #[test]
    fn the_output_is_behind_the_input_by_the_stated_delay() {
        for (from, to) in conversions() {
            let mut resampler = Resampler::new(from, to);
            // One click, half a frame in.
            let mut input = vec![0; from.frame() * 4];
            let click = from.frame() / 2;
            input[click] = 20_000;
            let mut out = vec![0; to.frame() * 4];
            resampler.process(&input, &mut out).unwrap();
            let peak = (0..out.len())
                .max_by_key(|&i| i32::from(out[i]).abs())
                .unwrap();
            let expected = from.length_of(click).ticks() + resampler.delay().ticks();
            let found = to.length_of(peak).ticks();
            // Within one sample of the lower rate.
            let slack = from.ticks_per_sample().max(to.ticks_per_sample());
            assert!(
                found.abs_diff(expected) <= slack,
                "{from:?} to {to:?}: the click is at tick {found}, expected {expected}"
            );
            assert!(resampler.delay().ticks() < 48 * 4, "under 4 ms");
        }
    }

    #[test]
    fn the_loudest_input_is_held_at_full_scale() {
        for (from, to) in conversions() {
            let probe = Resampler::new(from, to);
            let (up, down, taps) = (probe.up, probe.down, probe.taps);
            // The input a set of coefficients answers to most: full scale
            // with the sign of each coefficient. What it adds up to is
            // more than a sample can hold.
            let set = &probe.coefficients[..taps];
            let reach: i64 = set.iter().map(|&c| i64::from(c).abs()).sum();
            assert!(reach > i64::from(ONE), "{from:?} to {to:?}");
            // Zeros first, so that an output sample falls where that set
            // meets the whole of it.
            let pad = (0..down)
                .find(|pad| (pad + taps - 1) * up % down == 0)
                .unwrap();
            let at = (pad + taps - 1) * up / down;
            for (high, low) in [(i16::MAX, i16::MIN), (i16::MIN, i16::MAX)] {
                let mut input = vec![0; pad];
                input.extend(set.iter().map(|&c| if c >= 0 { high } else { low }));
                let mut resampler = Resampler::new(from, to);
                let mut out = vec![0; resampler.yields(input.len())];
                resampler.process(&input, &mut out).unwrap();
                assert_eq!(out[at], high, "{from:?} to {to:?}");
            }
        }
    }

    #[test]
    fn the_same_rate_passes_audio_through() {
        let mut resampler = Resampler::new(Rate::Hz16000, Rate::Hz16000);
        let input = tone(Rate::Hz16000, 440.0, 1_000);
        let mut out = vec![0; 1_000];
        assert_eq!(resampler.process(&input, &mut out), Ok(1_000));
        assert_eq!(out, input);
        assert_eq!(resampler.delay().ticks(), 0);
        assert_eq!(resampler.yields(77), 77);
    }

    #[test]
    fn a_reset_starts_a_new_stream() {
        let (from, to) = (Rate::Hz24000, Rate::Hz16000);
        let input = tone(from, 300.0, from.frame() + 5);
        let mut fresh = Resampler::new(from, to);
        let mut expected = vec![0; fresh.yields(input.len())];
        fresh.process(&input, &mut expected).unwrap();

        let mut used = Resampler::new(from, to);
        let mut scratch = vec![0; used.yields(333)];
        used.process(&tone(from, 900.0, 333), &mut scratch).unwrap();
        used.reset();
        let mut got = vec![0; used.yields(input.len())];
        used.process(&input, &mut got).unwrap();
        assert_eq!(got, expected);
    }
}
