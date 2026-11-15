//! Speech, for the tests that need something to be heard.
//!
//! A recogniser that is tested against its provider has to be sent
//! speech, and so has a model that listens for it. The speech here was
//! made by a machine: the free synthesiser Flite, with its voice `slt`,
//! run through `ffmpeg`:
//!
//! ```sh
//! ffmpeg -f lavfi -i "flite=text='What time is it right now?':voice=slt" \
//!     -ar 16000 -ac 1 -c:a pcm_s16le -bitexact -map_metadata -1 what-time-is-it.wav
//! ```
//!
//! Nobody was recorded for it, and nobody's voice is in this repository.

use waveport_adapter_kit::wav;
use waveport_media::resample::Resampler;
use waveport_ports::audio::Rate;

/// What the speech of [`what_time_is_it`] says.
pub const WHAT_TIME_IS_IT: &str = "What time is it right now?";

const WHAT_TIME_IS_IT_WAV: &[u8] = include_bytes!("../assets/what-time-is-it.wav");

/// A question of a second and three quarters, [`WHAT_TIME_IS_IT`], at
/// `rate`. It begins and ends without silence around it; see [`framed`].
///
/// # Panics
///
/// The file that is part of this crate cannot be read.
#[must_use]
#[expect(
    clippy::unwrap_used,
    reason = "a test whose own material is broken has failed"
)]
pub fn what_time_is_it(rate: Rate) -> Vec<i16> {
    let spoken = wav::read(WHAT_TIME_IS_IT_WAV).unwrap();
    let from = Rate::from_hz(spoken.hz).unwrap();
    if from == rate {
        return spoken.samples;
    }
    let mut resampler = Resampler::new(from, rate);
    // The filter gives each sample once it has seen what follows it, so
    // a little silence is put after the last one to bring it out.
    let mut input = spoken.samples;
    let length = input.len();
    input.extend(core::iter::repeat_n(
        0,
        from.samples_in(resampler.delay()) + 1,
    ));
    let mut out = vec![0; resampler.yields(input.len())];
    let written = resampler.process(&input, &mut out).unwrap();
    out.truncate(written);
    // The same filter puts as much silence before the first.
    let skip = rate.samples_in(resampler.delay());
    let wanted = length * (rate.hz() as usize) / (from.hz() as usize);
    out.drain(..skip.min(out.len()));
    out.truncate(wanted);
    out
}

/// `speech` with silence around it: `before` milliseconds of it first,
/// and `after` milliseconds at the end, as a person is quiet before and
/// after speaking.
#[must_use]
pub fn framed(rate: Rate, before: u32, speech: &[i16], after: u32) -> Vec<i16> {
    let lead = rate.samples_in_millis(before);
    let tail = rate.samples_in_millis(after);
    let mut audio = Vec::with_capacity(lead + speech.len() + tail);
    audio.resize(lead, 0);
    audio.extend_from_slice(speech);
    audio.resize(lead + speech.len() + tail, 0);
    audio
}

#[cfg(test)]
mod tests {
    use waveport_ports::audio::Rate;

    use super::{framed, what_time_is_it};

    /// How loud a stretch of audio is: the root of the mean of its
    /// samples squared.
    fn loudness(samples: &[i16]) -> f64 {
        let squares: f64 = samples.iter().map(|s| f64::from(*s).powi(2)).sum();
        #[expect(clippy::cast_precision_loss, reason = "a count of samples")]
        let count = samples.len().max(1) as f64;
        (squares / count).sqrt()
    }

    #[test]
    fn the_speech_is_as_long_and_as_loud_at_every_rate() {
        let spoken = what_time_is_it(Rate::Hz16000);
        // A second and three quarters, and not silence.
        assert_eq!(spoken.len(), 27_760);
        let loud = loudness(&spoken);
        assert!(loud > 1_000.0, "{loud}");
        for rate in Rate::ALL {
            let at_rate = what_time_is_it(rate);
            let expected = spoken.len() * rate.hz() as usize / 16_000;
            assert_eq!(at_rate.len(), expected, "{rate:?}");
            let ratio = loudness(&at_rate) / loud;
            assert!((0.9..1.1).contains(&ratio), "{rate:?}: {ratio}");
        }
    }

    #[test]
    fn silence_is_put_before_and_after() {
        let audio = framed(Rate::Hz8000, 100, &[5, 6, 7], 50);
        assert_eq!(audio.len(), 800 + 3 + 400);
        assert_eq!(audio[799..804], [0, 5, 6, 7, 0]);
    }
}
