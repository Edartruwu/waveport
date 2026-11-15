//! Audio in a WAV file: one channel of 16-bit samples.
//!
//! This is the only kind of file the project reads and writes audio in.
//! It is what a scripted session takes for a microphone and leaves as a
//! recording, and what a test has for speech.

use core::fmt;

/// The audio of a WAV file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wav {
    /// How many samples there are to a second.
    pub hz: u32,
    /// The samples.
    pub samples: Vec<i16>,
}

/// Why a file could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WavError {
    /// It is not a WAV file.
    NotWav,
    /// It ends before it says what it holds, or in the middle of that.
    CutShort,
    /// It holds something other than plain samples: compressed audio, or
    /// samples that are not whole numbers.
    NotPcm,
    /// It has this many channels, and one is all that is read.
    Channels(u16),
    /// Its samples have this many bits, and 16 is all that is read.
    Bits(u16),
}

impl fmt::Display for WavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotWav => f.write_str("not a WAV file"),
            Self::CutShort => f.write_str("a WAV file that is cut short"),
            Self::NotPcm => f.write_str("a WAV file of something other than plain samples"),
            Self::Channels(channels) => {
                write!(
                    f,
                    "a WAV file of {channels} channels: one is all that is read"
                )
            }
            Self::Bits(bits) => {
                write!(
                    f,
                    "a WAV file of {bits}-bit samples: 16 is all that is read"
                )
            }
        }
    }
}

impl core::error::Error for WavError {}

/// A number of two bytes, low byte first, at `at`.
fn two(bytes: &[u8], at: usize) -> Option<u16> {
    let pair = bytes.get(at..at + 2)?;
    Some(u16::from_le_bytes(pair.try_into().ok()?))
}

/// A number of four bytes, low byte first, at `at`.
fn four(bytes: &[u8], at: usize) -> Option<u32> {
    let quad = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes(quad.try_into().ok()?))
}

/// Reads a WAV file.
///
/// A file that was cut off in the middle of its samples gives the samples
/// it has: that is what a recording looks like when its writer was
/// stopped, and it is still a recording.
///
/// # Errors
///
/// The bytes are not a WAV file of one channel of 16-bit samples: see
/// [`WavError`].
pub fn read(bytes: &[u8]) -> Result<Wav, WavError> {
    if bytes.get(..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err(WavError::NotWav);
    }
    let mut hz = None;
    let mut at = 12;
    loop {
        let name = bytes.get(at..at + 4).ok_or(WavError::CutShort)?;
        let size = four(bytes, at + 4).ok_or(WavError::CutShort)?;
        let size = usize::try_from(size).map_err(|_| WavError::CutShort)?;
        let body = at + 8;
        match name {
            b"fmt " => {
                let format = two(bytes, body).ok_or(WavError::CutShort)?;
                let channels = two(bytes, body + 2).ok_or(WavError::CutShort)?;
                let rate = four(bytes, body + 4).ok_or(WavError::CutShort)?;
                let bits = two(bytes, body + 14).ok_or(WavError::CutShort)?;
                // 1 is plain samples. 0xFFFE says that the kind is written
                // further on, where 1 is plain samples again.
                let plain = match format {
                    1 => true,
                    0xFFFE => two(bytes, body + 24) == Some(1),
                    _ => false,
                };
                if !plain {
                    return Err(WavError::NotPcm);
                }
                if channels != 1 {
                    return Err(WavError::Channels(channels));
                }
                if bits != 16 {
                    return Err(WavError::Bits(bits));
                }
                hz = Some(rate);
            }
            b"data" => {
                let hz = hz.ok_or(WavError::NotWav)?;
                let end = body.saturating_add(size).min(bytes.len());
                let (pairs, _) = bytes.get(body..end).unwrap_or_default().as_chunks::<2>();
                let samples = pairs.iter().map(|pair| i16::from_le_bytes(*pair)).collect();
                return Ok(Wav { hz, samples });
            }
            _ => {}
        }
        // A part of odd size is followed by a byte that belongs to nothing.
        at = body.saturating_add(size).saturating_add(size % 2);
    }
}

/// Writes a WAV file of one channel of 16-bit samples.
#[must_use]
pub fn write(hz: u32, samples: &[i16]) -> Vec<u8> {
    // A file holds four thousand million bytes at most. More audio than
    // that is more than thirty hours of it, and the sizes then say less
    // than there is.
    let data = u32::try_from(samples.len() * 2).unwrap_or(u32::MAX - 36);
    let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes()); // Plain samples.
    bytes.extend_from_slice(&1_u16.to_le_bytes()); // One channel.
    bytes.extend_from_slice(&hz.to_le_bytes());
    bytes.extend_from_slice(&hz.saturating_mul(2).to_le_bytes()); // Bytes a second.
    bytes.extend_from_slice(&2_u16.to_le_bytes()); // Bytes a sample.
    bytes.extend_from_slice(&16_u16.to_le_bytes()); // Bits a sample.
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::{Wav, WavError, read, write};

    #[test]
    fn what_is_written_is_read_back() {
        let samples: Vec<i16> = (0..1_000).map(|n| n * 31 - 15_000).collect();
        let file = write(16_000, &samples);
        assert_eq!(file.len(), 44 + 2_000);
        assert_eq!(
            read(&file),
            Ok(Wav {
                hz: 16_000,
                samples
            })
        );
        assert_eq!(read(&write(48_000, &[])).unwrap().samples, []);
    }

    #[test]
    fn parts_that_are_not_audio_are_passed_over() {
        // As another program writes it: a note before the samples, of an
        // odd size, with the byte that follows such a part.
        let plain = write(24_000, &[1, -1, 300]);
        let mut file = plain[..36].to_vec();
        file.extend_from_slice(b"LIST");
        file.extend_from_slice(&5_u32.to_le_bytes());
        file.extend_from_slice(b"notes\0");
        file.extend_from_slice(&plain[36..]);
        let wav = read(&file).unwrap();
        assert_eq!((wav.hz, wav.samples), (24_000, vec![1, -1, 300]));
    }

    #[test]
    fn a_recording_that_was_cut_off_gives_what_it_has() {
        let file = write(16_000, &[10, 20, 30, 40]);
        // Its size says four samples; two and a half are there.
        let wav = read(&file[..44 + 5]).unwrap();
        assert_eq!(wav.samples, [10, 20]);
    }

    #[test]
    fn what_cannot_be_read_says_why() {
        assert_eq!(read(b"not a wav file at all"), Err(WavError::NotWav));
        assert_eq!(read(b""), Err(WavError::NotWav));
        let good = write(16_000, &[1, 2]);
        // Cut before the samples are announced.
        assert_eq!(read(&good[..30]), Err(WavError::CutShort));

        let mut stereo = good.clone();
        stereo[22] = 2;
        assert_eq!(read(&stereo), Err(WavError::Channels(2)));
        let mut bytes = good.clone();
        bytes[34] = 8;
        assert_eq!(read(&bytes), Err(WavError::Bits(8)));
        let mut float = good.clone();
        float[20] = 3;
        assert_eq!(read(&float), Err(WavError::NotPcm));
        // Samples before anything says what they are.
        let mut headless = good[..12].to_vec();
        headless.extend_from_slice(&good[36..]);
        assert_eq!(read(&headless), Err(WavError::NotWav));
        assert!(WavError::Channels(2).to_string().contains("2 channels"));
    }

    #[test]
    fn the_long_form_of_plain_samples_is_read() {
        // The form with the kind written further on, as some programs
        // write every file.
        let mut file = Vec::new();
        file.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        file.extend_from_slice(&40_u32.to_le_bytes());
        file.extend_from_slice(&0xFFFE_u16.to_le_bytes());
        file.extend_from_slice(&1_u16.to_le_bytes());
        file.extend_from_slice(&8_000_u32.to_le_bytes());
        file.extend_from_slice(&16_000_u32.to_le_bytes());
        file.extend_from_slice(&2_u16.to_le_bytes());
        file.extend_from_slice(&16_u16.to_le_bytes());
        file.extend_from_slice(&22_u16.to_le_bytes()); // More follows.
        file.extend_from_slice(&16_u16.to_le_bytes()); // Bits that count.
        file.extend_from_slice(&4_u32.to_le_bytes()); // Where it plays.
        file.extend_from_slice(&1_u16.to_le_bytes()); // Plain samples.
        file.extend_from_slice(&[0; 14]);
        file.extend_from_slice(b"data");
        file.extend_from_slice(&4_u32.to_le_bytes());
        file.extend_from_slice(&[7, 0, 9, 0]);
        let wav = read(&file).unwrap();
        assert_eq!((wav.hz, wav.samples), (8_000, vec![7, 9]));
    }
}
