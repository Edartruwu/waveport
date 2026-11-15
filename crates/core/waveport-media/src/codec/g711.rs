//! G.711: the telephone network's two ways of writing a sample in one
//! byte, at 8 kHz.
//!
//! Both keep small samples fine and large ones coarse. Decoding is a table
//! of 256 entries; encoding finds the sample's segment and takes four bits
//! of what is left.

pub use waveport_domain::audio::Law;

/// What µ-law adds to a sample before it looks for the segment.
const MU_BIAS: i32 = 0x84;
/// The largest sample µ-law can hold once the bias is added.
const MU_CLIP: i32 = 32_635;

#[expect(
    clippy::cast_possible_truncation,
    reason = "the magnitude is 32,124 at most"
)]
const fn mu_to_linear(byte: u8) -> i16 {
    let byte = !byte;
    let exponent = (byte >> 4) & 0x07;
    let mantissa = (byte & 0x0F) as i32;
    let magnitude = (((mantissa << 3) + MU_BIAS) << exponent) - MU_BIAS;
    if byte & 0x80 == 0 {
        magnitude as i16
    } else {
        -(magnitude as i16)
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "the magnitude is 32,256 at most"
)]
const fn a_to_linear(byte: u8) -> i16 {
    let byte = byte ^ 0x55;
    let segment = (byte >> 4) & 0x07;
    let mantissa = ((byte & 0x0F) as i32) << 4;
    let magnitude = match segment {
        0 => mantissa + 8,
        1 => mantissa + 0x108,
        _ => (mantissa + 0x108) << (segment - 1),
    };
    if byte & 0x80 == 0 {
        -(magnitude as i16)
    } else {
        magnitude as i16
    }
}

#[expect(clippy::cast_possible_truncation, reason = "the index is below 256")]
#[expect(
    clippy::indexing_slicing,
    reason = "evaluated when the crate is compiled, where an index out of range is an error"
)]
const fn table(law: Law) -> [i16; 256] {
    let mut table = [0; 256];
    let mut byte = 0;
    while byte < 256 {
        table[byte] = match law {
            Law::Mu => mu_to_linear(byte as u8),
            Law::A => a_to_linear(byte as u8),
        };
        byte += 1;
    }
    table
}

static MU_TABLE: [i16; 256] = table(Law::Mu);
static A_TABLE: [i16; 256] = table(Law::A);

/// The sample a byte stands for.
#[must_use]
pub fn to_linear(law: Law, byte: u8) -> i16 {
    let table = match law {
        Law::Mu => &MU_TABLE,
        Law::A => &A_TABLE,
    };
    table.get(usize::from(byte)).copied().unwrap_or(0)
}

/// The number of the segment `magnitude` falls in, of eight. The first
/// ends before `first_end`, and each of the others at twice the end of the
/// one before it. 8 when the magnitude is past the last.
const fn segment_of(magnitude: i32, first_end: i32) -> u8 {
    let mut segment = 0;
    while segment < 8 {
        if magnitude < first_end << segment {
            return segment;
        }
        segment += 1;
    }
    8
}

/// The byte that stands for a sample.
#[must_use]
#[expect(
    clippy::cast_sign_loss,
    reason = "four bits of a magnitude that is not negative"
)]
pub const fn from_linear(law: Law, sample: i16) -> u8 {
    let sample = sample as i32;
    match law {
        Law::Mu => {
            let (sign, magnitude) = if sample < 0 {
                (0x80, -sample)
            } else {
                (0x00, sample)
            };
            let magnitude = if magnitude > MU_CLIP {
                MU_CLIP
            } else {
                magnitude
            } + MU_BIAS;
            // With the bias the magnitude is between 0x84 and 0x7FFF, so
            // it falls in one of the eight segments.
            let exponent = segment_of(magnitude, 0x100);
            let mantissa = ((magnitude >> (exponent + 3)) & 0x0F) as u8;
            !(sign | (exponent << 4) | mantissa)
        }
        Law::A => {
            // A-law works on 13 bits.
            let sample = sample >> 3;
            let (mask, magnitude) = if sample >= 0 {
                (0xD5, sample)
            } else {
                (0x55, -sample - 1)
            };
            let segment = segment_of(magnitude, 0x20);
            if segment >= 8 {
                return 0x7F ^ mask;
            }
            let shift = if segment < 2 { 1 } else { segment };
            let mantissa = ((magnitude >> shift) & 0x0F) as u8;
            ((segment << 4) | mantissa) ^ mask
        }
    }
}

/// Decodes `bytes` into the start of `out`, as far as `out` has room, and
/// returns how many samples it wrote.
pub fn decode(law: Law, bytes: &[u8], out: &mut [i16]) -> usize {
    let table = match law {
        Law::Mu => &MU_TABLE,
        Law::A => &A_TABLE,
    };
    for (sample, &byte) in out.iter_mut().zip(bytes) {
        *sample = table.get(usize::from(byte)).copied().unwrap_or(0);
    }
    bytes.len().min(out.len())
}

/// Encodes `pcm` into the start of `out`, as far as `out` has room, and
/// returns how many bytes it wrote.
pub fn encode(law: Law, pcm: &[i16], out: &mut [u8]) -> usize {
    for (byte, &sample) in out.iter_mut().zip(pcm) {
        *byte = from_linear(law, sample);
    }
    pcm.len().min(out.len())
}

#[cfg(test)]
mod tests {
    use super::{Law, decode, encode, from_linear, to_linear};
    use crate::rate::Rate;
    use crate::testing::{energy, tone};

    #[test]
    fn the_ends_and_the_middle_of_each_law_are_where_the_standard_puts_them() {
        // µ-law: 0xFF and 0x7F are the two zeros, 0x80 and 0x00 the peaks.
        assert_eq!(to_linear(Law::Mu, 0xFF), 0);
        assert_eq!(to_linear(Law::Mu, 0x7F), 0);
        assert_eq!(to_linear(Law::Mu, 0x80), 32_124);
        assert_eq!(to_linear(Law::Mu, 0x00), -32_124);
        assert_eq!(to_linear(Law::Mu, 0xFE), 8, "the first step up");
        assert_eq!(
            to_linear(Law::Mu, 0xEF),
            132,
            "the top of segment 0, plus one step"
        );
        assert_eq!(from_linear(Law::Mu, 0), 0xFF);
        assert_eq!(from_linear(Law::Mu, -1), 0x7F);
        assert_eq!(from_linear(Law::Mu, i16::MAX), 0x80);
        assert_eq!(from_linear(Law::Mu, i16::MIN), 0x00);

        // A-law has no zero: 0xD5 and 0x55 are the smallest steps.
        assert_eq!(to_linear(Law::A, 0xD5), 8);
        assert_eq!(to_linear(Law::A, 0x55), -8);
        assert_eq!(to_linear(Law::A, 0xAA), 32_256);
        assert_eq!(to_linear(Law::A, 0x2A), -32_256);
        assert_eq!(from_linear(Law::A, 0), 0xD5);
        assert_eq!(from_linear(Law::A, -1), 0x55);
        assert_eq!(from_linear(Law::A, i16::MAX), 0xAA);
        assert_eq!(from_linear(Law::A, i16::MIN), 0x2A);
    }

    #[test]
    fn every_byte_comes_back_as_itself() {
        for law in [Law::Mu, Law::A] {
            for byte in 0..=u8::MAX {
                let again = from_linear(law, to_linear(law, byte));
                // µ-law has two zeros, and writes zero as the positive one.
                let expected = if law == Law::Mu && byte == 0x7F {
                    0xFF
                } else {
                    byte
                };
                assert_eq!(again, expected, "{law:?} {byte:#04x}");
            }
        }
    }

    #[test]
    fn a_sample_comes_back_close_and_never_out_of_order() {
        for law in [Law::Mu, Law::A] {
            let mut last = i16::MIN;
            for sample in i16::MIN..=i16::MAX {
                let back = to_linear(law, from_linear(law, sample));
                assert!(back >= last, "{law:?} {sample}: {back} after {last}");
                last = back;
                // A step is a sixteenth of the sample's segment at most,
                // and no finer than 16; the ends are clipped.
                let off = (i32::from(back) - i32::from(sample)).abs();
                let bound = (i32::from(sample).abs() / 16).max(16);
                assert!(
                    off <= bound || i32::from(sample).abs() > 32_000,
                    "{law:?} {sample}: {back}"
                );
            }
        }
    }

    #[test]
    fn a_tone_keeps_its_shape() {
        let pcm = tone(Rate::Hz8000, 440.0, 1_600);
        for law in [Law::Mu, Law::A] {
            let mut bytes = [0; 1_600];
            assert_eq!(encode(law, &pcm, &mut bytes), 1_600);
            let mut back = [0; 1_600];
            assert_eq!(decode(law, &bytes, &mut back), 1_600);
            let noise: Vec<i16> = pcm.iter().zip(&back).map(|(a, b)| a - b).collect();
            // G.711 gives about 38 dB between signal and noise.
            assert!(energy(&pcm) / energy(&noise) > 3_000.0, "{law:?}");
        }
    }

    #[test]
    fn only_as_much_is_written_as_there_is_room_for() {
        let mut out = [0; 2];
        assert_eq!(decode(Law::Mu, &[0x80, 0x00, 0xFF], &mut out), 2);
        assert_eq!(out, [32_124, -32_124]);
        let mut bytes = [0; 4];
        assert_eq!(encode(Law::A, &[0, -1], &mut bytes), 2);
        assert_eq!(bytes, [0xD5, 0x55, 0, 0]);
    }
}
