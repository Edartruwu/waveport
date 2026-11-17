//! The tones of a telephone's keys.
//!
//! A key of a telephone sounds as two sine waves at once: one for its row
//! on the keypad and one for its column (ITU-T Q.23, "DTMF"). A session
//! that has to press keys at the far end, for a menu that asks for a
//! number, asks its transport to. A transport that has no way of its own
//! leaves the audio: the tones are made here and added to what the
//! session sends, over whatever is being said, as on a telephone.
//!
//! Nothing here listens for tones. Keys that somebody presses reach a
//! session from its transport as events, and never from the audio: speech
//! is too easily taken for a key.
//!
//! The tones are made with whole numbers alone, from a table of a quarter
//! of a sine wave and a phase counted in 2^32 parts of a turn, so that a
//! key gives the same samples on every machine.

use crate::rate::Rate;
use waveport_domain::event::Key;

/// How long the tone of a key lasts, in milliseconds. A receiver has to
/// take a tone of 40 ms for a key (ITU-T Q.24), and none asks for more
/// than this.
pub const TONE_MS: u32 = 100;

/// How long nothing sounds after the tone of a key, in milliseconds,
/// before the next may begin. Receivers ask for between 30 and 70 ms,
/// and for a tone and its pause to last 93 to 125 ms together.
pub const PAUSE_MS: u32 = 100;

/// How many keys can wait to be pressed.
pub const MOST: usize = 32;

/// The tone of a key's row at its loudest: 8 dB under the level the
/// telephone network measures from, where the loudest sine wave of G.711
/// is 3.17 dB over it.
const ROW_PEAK: i32 = 9_056;

/// The tone of a key's column at its loudest: 2 dB more than its row's,
/// because a line takes more from the higher tone.
const COLUMN_PEAK: i32 = 11_401;

/// The first quarter of a sine wave whose peak is 32,767, in 256 steps
/// and the peak itself.
const QUARTER: [i16; 257] = [
    0, 201, 402, 603, 804, 1005, 1206, 1407, 1608, 1809, 2009, 2210, 2410, 2611, 2811, 3012, 3212,
    3412, 3612, 3811, 4011, 4210, 4410, 4609, 4808, 5007, 5205, 5404, 5602, 5800, 5998, 6195, 6393,
    6590, 6786, 6983, 7179, 7375, 7571, 7767, 7962, 8157, 8351, 8545, 8739, 8933, 9126, 9319, 9512,
    9704, 9896, 10087, 10278, 10469, 10659, 10849, 11039, 11228, 11417, 11605, 11793, 11980, 12167,
    12353, 12539, 12725, 12910, 13094, 13279, 13462, 13645, 13828, 14010, 14191, 14372, 14553,
    14732, 14912, 15090, 15269, 15446, 15623, 15800, 15976, 16151, 16325, 16499, 16673, 16846,
    17018, 17189, 17360, 17530, 17700, 17869, 18037, 18204, 18371, 18537, 18703, 18868, 19032,
    19195, 19357, 19519, 19680, 19841, 20000, 20159, 20317, 20475, 20631, 20787, 20942, 21096,
    21250, 21403, 21554, 21705, 21856, 22005, 22154, 22301, 22448, 22594, 22739, 22884, 23027,
    23170, 23311, 23452, 23592, 23731, 23870, 24007, 24143, 24279, 24413, 24547, 24680, 24811,
    24942, 25072, 25201, 25329, 25456, 25582, 25708, 25832, 25955, 26077, 26198, 26319, 26438,
    26556, 26674, 26790, 26905, 27019, 27133, 27245, 27356, 27466, 27575, 27683, 27790, 27896,
    28001, 28105, 28208, 28310, 28411, 28510, 28609, 28706, 28803, 28898, 28992, 29085, 29177,
    29268, 29358, 29447, 29534, 29621, 29706, 29791, 29874, 29956, 30037, 30117, 30195, 30273,
    30349, 30424, 30498, 30571, 30643, 30714, 30783, 30852, 30919, 30985, 31050, 31113, 31176,
    31237, 31297, 31356, 31414, 31470, 31526, 31580, 31633, 31685, 31736, 31785, 31833, 31880,
    31926, 31971, 32014, 32057, 32098, 32137, 32176, 32213, 32250, 32285, 32318, 32351, 32382,
    32412, 32441, 32469, 32495, 32521, 32545, 32567, 32589, 32609, 32628, 32646, 32663, 32678,
    32692, 32705, 32717, 32728, 32737, 32745, 32752, 32757, 32761, 32765, 32766, 32767,
];

/// The parts of a turn in one step of the table: 2^22.
const STEP: u32 = 1 << 22;

/// The frequencies of a key, in hertz: its row's and its column's.
#[must_use]
pub const fn frequencies(key: Key) -> (u32, u32) {
    let row = match key {
        Key::One | Key::Two | Key::Three | Key::A => 697,
        Key::Four | Key::Five | Key::Six | Key::B => 770,
        Key::Seven | Key::Eight | Key::Nine | Key::C => 852,
        Key::Star | Key::Zero | Key::Hash | Key::D => 941,
    };
    let column = match key {
        Key::One | Key::Four | Key::Seven | Key::Star => 1_209,
        Key::Two | Key::Five | Key::Eight | Key::Zero => 1_336,
        Key::Three | Key::Six | Key::Nine | Key::Hash => 1_477,
        Key::A | Key::B | Key::C | Key::D => 1_633,
    };
    (row, column)
}

/// The sine of `phase`, which counts a turn in 2^32 parts, where the
/// peak is 32,767.
fn sine(phase: u32) -> i32 {
    let quadrant = phase >> 30;
    let within = phase & ((1 << 30) - 1);
    // The second and the fourth quarter are the first, read backwards.
    let within = if quadrant & 1 == 1 {
        (1 << 30) - within
    } else {
        within
    };
    let index = usize::try_from(within / STEP).unwrap_or(0);
    let at = |index: usize| i64::from(QUARTER.get(index).copied().unwrap_or(i16::MAX));
    let (from, to) = (at(index), at(index + 1));
    let part = i64::from(within % STEP);
    let step = i64::from(STEP);
    let value = from + ((to - from) * part + step / 2) / step;
    let value = i32::try_from(value).unwrap_or(0);
    if quadrant >= 2 { -value } else { value }
}

/// How far the phase of a tone of `hz` moves from one sample to the
/// next at `rate`.
fn advance(hz: u32, rate: Rate) -> u32 {
    let rate = u64::from(rate.hz());
    let turn = (u64::from(hz) << 32) + rate / 2;
    // Less than a turn: no key sounds at half the lowest rate or more.
    u32::try_from(turn / rate).unwrap_or(0)
}

/// Presses keys: makes their tones, one after the other.
#[derive(Clone, Debug)]
pub struct Dialer {
    rate: Rate,
    /// The keys that are to be pressed, the one that is being pressed
    /// first, from `first` on and round.
    waiting: [Key; MOST],
    first: usize,
    count: usize,
    /// How many samples of the key that is being pressed have been made,
    /// of its tone and of the pause after it.
    made: usize,
    /// Where its two tones are in their turns.
    row: u32,
    column: u32,
}

impl Dialer {
    /// A dialer for audio at `rate`, with no key to press.
    #[must_use]
    pub const fn new(rate: Rate) -> Self {
        Self {
            rate,
            waiting: [Key::Zero; MOST],
            first: 0,
            count: 0,
            made: 0,
            row: 0,
            column: 0,
        }
    }

    /// The rate of the audio it makes.
    #[must_use]
    pub const fn rate(&self) -> Rate {
        self.rate
    }

    /// Whether no key is being pressed and none waits to be.
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        self.count == 0
    }

    /// How many keys are still to be pressed, with the one that is being
    /// pressed.
    #[must_use]
    pub const fn waiting(&self) -> usize {
        self.count
    }

    /// How many of them have not sounded in full: all, but for the one
    /// that is being pressed once its tone is over and only the pause
    /// after it is left.
    #[must_use]
    pub const fn unsounded(&self) -> usize {
        let sounded = self.count > 0 && self.made >= self.rate.samples_in_millis(TONE_MS);
        self.count - sounded as usize
    }

    /// Presses `key` when the keys before it have been pressed. Returns
    /// whether there was room for it: [`MOST`] keys can wait.
    #[must_use]
    pub fn press(&mut self, key: Key) -> bool {
        let Some(slot) = self.waiting.get_mut((self.first + self.count) % MOST) else {
            return false;
        };
        if self.count == MOST {
            return false;
        }
        *slot = key;
        self.count += 1;
        true
    }

    /// Forgets the keys that are still to be pressed, and stops the one
    /// that is being pressed where it is.
    pub const fn clear(&mut self) {
        self.count = 0;
        self.made = 0;
    }

    /// Adds what is being pressed to `audio`: the tone of each key and
    /// the pause after it, over whatever `audio` holds. Returns for how
    /// many of its samples a key was being pressed, from the first on.
    pub fn mix(&mut self, audio: &mut [i16]) -> usize {
        let tone = self.rate.samples_in_millis(TONE_MS);
        let whole = tone + self.rate.samples_in_millis(PAUSE_MS);
        // The tone begins and ends over 2.5 ms, and not at once: an edge
        // is a click, which has every frequency in it.
        let edge = (tone / 40).max(1);
        let mut pressed = 0;
        for sample in audio {
            let Some(&key) = self.waiting.get(self.first) else {
                break;
            };
            if self.count == 0 {
                break;
            }
            pressed += 1;
            if self.made < tone {
                if self.made == 0 {
                    (self.row, self.column) = (0, 0);
                }
                let (row, column) = frequencies(key);
                let both = ROW_PEAK * sine(self.row) + COLUMN_PEAK * sine(self.column);
                let level = (self.made + 1).min(tone - self.made).min(edge);
                let level = i64::try_from(level).unwrap_or(0);
                let edge = i64::try_from(edge).unwrap_or(1);
                let value = i64::from(both / i32::from(i16::MAX)) * level / edge;
                let value = i16::try_from(value).unwrap_or(0);
                *sample = sample.saturating_add(value);
                self.row = self.row.wrapping_add(advance(row, self.rate));
                self.column = self.column.wrapping_add(advance(column, self.rate));
            }
            self.made += 1;
            if self.made == whole {
                self.made = 0;
                self.first = (self.first + 1) % MOST;
                self.count -= 1;
            }
        }
        pressed
    }
}

#[cfg(test)]
mod tests {
    use core::f64::consts::TAU;

    use waveport_domain::event::Key;

    use super::{COLUMN_PEAK, Dialer, MOST, PAUSE_MS, QUARTER, ROW_PEAK, TONE_MS, frequencies};
    use super::{advance, sine};
    use crate::rate::Rate;

    const ROWS: [u32; 4] = [697, 770, 852, 941];
    const COLUMNS: [u32; 4] = [1_209, 1_336, 1_477, 1_633];

    /// What pressing `keys` sounds like at `rate`, to the end of the last
    /// pause.
    fn pressed(rate: Rate, keys: &str) -> Vec<i16> {
        let mut dialer = Dialer::new(rate);
        for key in keys.chars() {
            assert!(dialer.press(Key::from_char(key).unwrap()));
        }
        let each = rate.samples_in_millis(TONE_MS + PAUSE_MS);
        let mut audio = vec![0; each * keys.len()];
        assert_eq!(dialer.mix(&mut audio), audio.len());
        assert!(dialer.is_idle());
        audio
    }

    /// How loud the sine wave of `hz` is in `audio`: its peak. The audio
    /// is weighed down towards its ends, so that a tone 73 Hz away, which
    /// the nearest is, does not count.
    fn peak(audio: &[i16], rate: Rate, hz: u32) -> f64 {
        let turn = TAU * f64::from(hz) / f64::from(rate.hz());
        let length = f64::from(u32::try_from(audio.len()).unwrap());
        let (mut sin, mut cos, mut weights) = (0.0, 0.0, 0.0);
        for (n, &sample) in (0_u32..).zip(audio) {
            let weight = 0.5 - 0.5 * (TAU * f64::from(n) / length).cos();
            let at = turn * f64::from(n);
            sin += weight * f64::from(sample) * at.sin();
            cos += weight * f64::from(sample) * at.cos();
            weights += weight;
        }
        2.0 * sin.hypot(cos) / weights
    }

    #[test]
    fn the_keys_sound_as_the_keypad_is_laid_out() {
        let keypad = ["123A", "456B", "789C", "*0#D"];
        for (row, keys) in ROWS.iter().zip(keypad) {
            for (column, key) in COLUMNS.iter().zip(keys.chars()) {
                let key = Key::from_char(key).unwrap();
                assert_eq!(frequencies(key), (*row, *column), "{key:?}");
            }
        }
    }

    #[test]
    fn the_table_is_a_quarter_of_a_sine_wave() {
        for (step, &value) in (0_u32..).zip(&QUARTER) {
            let sine = (TAU / 4.0 * f64::from(step) / 256.0).sin();
            let off = (f64::from(value) - 32_767.0 * sine).abs();
            assert!(off <= 0.5 + 1e-6, "{step}: {value} for {sine}");
        }
        // And the wave that is read from it is one, all the way round.
        for part in 0..4_096_u32 {
            let phase = part << 20;
            let wanted = 32_767.0 * (TAU * f64::from(part) / 4_096.0).sin();
            let off = (f64::from(sine(phase)) - wanted).abs();
            assert!(off <= 1.0, "{part}: {} for {wanted}", sine(phase));
        }
        assert_eq!(sine(0), 0);
        assert_eq!(sine(1 << 30), 32_767);
        assert_eq!(sine(1 << 31), 0);
        assert_eq!(sine(3 << 30), -32_767);
    }

    #[test]
    fn every_key_is_its_two_tones_and_no_other_at_every_rate() {
        for rate in Rate::ALL {
            for key in Key::ALL {
                let audio = pressed(rate, &key.as_char().to_string());
                let tone = rate.samples_in_millis(TONE_MS);
                // The middle of the tone, where it is as loud as it gets.
                let edge = tone / 10;
                let middle = &audio[edge..tone - edge];
                let (row, column) = frequencies(key);
                for hz in ROWS.into_iter().chain(COLUMNS) {
                    let loud = peak(middle, rate, hz);
                    let wanted = if hz == row {
                        f64::from(ROW_PEAK)
                    } else if hz == column {
                        f64::from(COLUMN_PEAK)
                    } else {
                        0.0
                    };
                    // Within a hundredth of the quieter tone.
                    let off = (loud - wanted).abs();
                    assert!(
                        off <= 0.01 * f64::from(ROW_PEAK),
                        "{rate:?} {key:?} {hz}: {loud}"
                    );
                }
                // The column's tone is 2 dB over the row's.
                let twist = 20.0 * (peak(middle, rate, column) / peak(middle, rate, row)).log10();
                assert!((twist - 2.0).abs() < 0.1, "{rate:?} {key:?}: {twist}");
            }
        }
    }

    #[test]
    fn every_sample_is_where_the_two_sine_waves_are() {
        for rate in Rate::ALL {
            let audio = pressed(rate, "5");
            let tone = rate.samples_in_millis(TONE_MS);
            let edge = tone / 40;
            let (row, column) = frequencies(Key::Five);
            let wave =
                |hz: u32, n: u32| (TAU * f64::from(hz) * f64::from(n) / f64::from(rate.hz())).sin();
            for (n, &sample) in (0_u32..).zip(&audio).take(tone).skip(edge) {
                if usize::try_from(n).unwrap() >= tone - edge {
                    break;
                }
                let wanted =
                    f64::from(ROW_PEAK) * wave(row, n) + f64::from(COLUMN_PEAK) * wave(column, n);
                // The phase of a tone is a whole number of 2^32 parts of
                // a turn, and is off by a part in four thousand million
                // each sample at most.
                let off = (f64::from(sample) - wanted).abs();
                assert!(off <= 2.5, "{rate:?} sample {n}: {sample} for {wanted}");
            }
        }
    }

    #[test]
    fn a_key_is_the_same_samples_on_every_machine() {
        // The first samples of the one at 8 kHz, and a sum over the whole
        // of every key at every rate.
        let one = pressed(Rate::Hz8000, "1");
        assert_eq!(one[..8], FIRST_OF_ONE);
        let mut sums = Vec::new();
        for rate in Rate::ALL {
            let mut sum = 0_u64;
            for key in Key::ALL {
                for &sample in &pressed(rate, &key.as_char().to_string()) {
                    sum = sum
                        .wrapping_mul(31)
                        .wrapping_add(u64::from(sample.unsigned_abs()));
                    sum ^= u64::from(sample < 0);
                }
            }
            sums.push(sum);
        }
        assert_eq!(sums, SUMS);
    }

    const FIRST_OF_ONE: [i16; 8] = [0, 1_398, 2_826, 2_465, 104, -2_349, -2_657, -684];
    const SUMS: [u64; 4] = [
        3_399_711_656_512_736_476,
        436_131_850_586_438_117,
        3_249_929_446_401_536_143,
        8_044_120_832_482_560_446,
    ];

    #[test]
    fn a_tone_lasts_as_long_as_it_says_and_a_pause_follows_it() {
        for rate in Rate::ALL {
            let audio = pressed(rate, "58");
            let tone = rate.samples_in_millis(TONE_MS);
            let pause = rate.samples_in_millis(PAUSE_MS);
            assert_eq!(audio.len(), 2 * (tone + pause));
            let (first, rest) = audio.split_at(tone + pause);
            assert!(first[tone..].iter().all(|&sample| sample == 0), "{rate:?}");
            assert!(rest[tone..].iter().all(|&sample| sample == 0), "{rate:?}");
            // A tone is loud to its end, and begins and ends softly.
            let loud = |audio: &[i16]| {
                audio
                    .iter()
                    .map(|sample| sample.unsigned_abs())
                    .max()
                    .unwrap()
            };
            assert!(loud(&first[tone - tone / 10..tone]) > 10_000, "{rate:?}");
            assert!(loud(&first[..tone]) <= 20_457, "{rate:?}");
            assert!(first[0].unsigned_abs() < 1_000 && first[tone - 1].unsigned_abs() < 1_000);
            // The same key is the same samples wherever it is in a run.
            assert_eq!(pressed(rate, "85")[tone + pause..], first[..]);
        }
    }

    #[test]
    fn tones_are_the_same_however_the_audio_is_cut() {
        let whole = pressed(Rate::Hz16000, "4071#");
        for frame in [1, 160, 320, 333, 5_000] {
            let mut dialer = Dialer::new(Rate::Hz16000);
            for key in "4071#".chars() {
                assert!(dialer.press(Key::from_char(key).unwrap()));
            }
            let mut audio = vec![0; whole.len() + 1_000];
            let mut done = 0;
            for piece in audio.chunks_mut(frame) {
                done += dialer.mix(piece);
            }
            assert_eq!(done, whole.len(), "frames of {frame}");
            assert_eq!(audio[..whole.len()], whole[..], "frames of {frame}");
            assert!(audio[whole.len()..].iter().all(|&sample| sample == 0));
        }
    }

    #[test]
    fn a_tone_sounds_over_what_is_being_said() {
        let alone = pressed(Rate::Hz8000, "9");
        let mut dialer = Dialer::new(Rate::Hz8000);
        assert!(dialer.press(Key::Nine));
        let mut audio = vec![1_000_i16; alone.len()];
        dialer.mix(&mut audio);
        for (both, tone) in audio.iter().zip(&alone) {
            assert_eq!(*both, tone + 1_000);
        }
        // What is too loud for a sample is as loud as a sample gets.
        assert!(dialer.press(Key::Nine));
        let mut audio = vec![i16::MAX; alone.len()];
        dialer.mix(&mut audio);
        assert!(
            audio
                .iter()
                .zip(&alone)
                .all(|(both, tone)| { *both == i16::MAX.saturating_add(*tone) })
        );
        assert!(audio.contains(&i16::MAX));
    }

    #[test]
    fn keys_wait_their_turn_and_only_so_many() {
        let mut dialer = Dialer::new(Rate::Hz8000);
        assert!(dialer.is_idle());
        // Nothing is pressed: the audio is left as it is.
        let mut audio = [7_i16; 160];
        assert_eq!(dialer.mix(&mut audio), 0);
        assert_eq!(audio, [7; 160]);

        for _ in 0..MOST {
            assert!(dialer.press(Key::Star));
        }
        assert!(
            !dialer.press(Key::Star),
            "the key after the last there is room for"
        );
        assert_eq!(dialer.waiting(), MOST);
        assert_eq!(dialer.unsounded(), MOST);
        // A key has sounded when its tone is over, and is pressed when
        // the pause after it is.
        let each = Rate::Hz8000.samples_in_millis(TONE_MS + PAUSE_MS);
        let tone = Rate::Hz8000.samples_in_millis(TONE_MS);
        let mut audio = vec![0; each];
        assert_eq!(dialer.mix(&mut audio[..tone - 1]), tone - 1);
        assert_eq!((dialer.waiting(), dialer.unsounded()), (MOST, MOST));
        assert_eq!(dialer.mix(&mut audio[tone - 1..tone]), 1);
        assert_eq!((dialer.waiting(), dialer.unsounded()), (MOST, MOST - 1));
        // One key is pressed, and there is room for one more.
        assert_eq!(dialer.mix(&mut audio[tone..]), each - tone);
        assert_eq!((dialer.waiting(), dialer.unsounded()), (MOST - 1, MOST - 1));
        assert!(dialer.press(Key::Hash));
        assert!(!dialer.press(Key::Hash));
        // The last of them is the hash, after all the stars.
        let mut audio = vec![0; each * MOST];
        assert_eq!(dialer.mix(&mut audio), each * MOST);
        assert!(dialer.is_idle());
        let last = &audio[each * (MOST - 1)..];
        assert_eq!(last, &pressed(Rate::Hz8000, "#")[..]);

        // Keys that are forgotten are not pressed.
        assert!(dialer.press(Key::One) && dialer.press(Key::Two));
        let mut audio = vec![0; 80];
        assert_eq!(dialer.mix(&mut audio), 80);
        dialer.clear();
        assert!(dialer.is_idle());
        let mut audio = vec![0; 80];
        assert_eq!(dialer.mix(&mut audio), 0);
        assert_eq!(audio, vec![0; 80]);
        // And the next key begins at its beginning.
        assert!(dialer.press(Key::Three));
        let mut audio = vec![0; each];
        dialer.mix(&mut audio);
        assert_eq!(audio, pressed(Rate::Hz8000, "3"));
    }

    #[test]
    fn a_phase_moves_by_the_share_of_a_turn_that_a_sample_is() {
        // 1,000 Hz at 8 kHz is an eighth of a turn each sample.
        assert_eq!(advance(1_000, Rate::Hz8000), 1 << 29);
        assert_eq!(advance(941, Rate::Hz48000), 84_199_255);
    }
}
