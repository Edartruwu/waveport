//! Test audio: a WAV reader and Opus packets encoded ahead of the run.

use std::path::Path;

use crate::Result;

/// Mono 16-bit audio.
#[derive(Clone, Debug)]
pub struct Wav {
    pub rate: u32,
    pub samples: Vec<i16>,
}

impl Wav {
    /// The samples as floats in `[-1, 1)`.
    #[must_use]
    pub fn to_f32(&self) -> Vec<f32> {
        self.samples
            .iter()
            .map(|&s| f32::from(s) / 32768.0)
            .collect()
    }
}

fn le_u16(bytes: &[u8], at: usize) -> Result<u16> {
    let pair = bytes.get(at..at + 2).ok_or("truncated WAV file")?;
    Ok(u16::from_le_bytes([pair[0], pair[1]]))
}

fn le_u32(bytes: &[u8], at: usize) -> Result<u32> {
    let quad = bytes.get(at..at + 4).ok_or("truncated WAV file")?;
    Ok(u32::from_le_bytes([quad[0], quad[1], quad[2], quad[3]]))
}

/// Reads a mono, 16-bit PCM WAV file.
pub fn read_wav(path: &Path) -> Result<Wav> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err(format!("{}: not a WAV file", path.display()).into());
    }
    let mut at = 12;
    let mut rate = None;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = le_u32(&bytes, at + 4)? as usize;
        let body = at + 8;
        if id == b"fmt " {
            let format = le_u16(&bytes, body)?;
            let channels = le_u16(&bytes, body + 2)?;
            let bits = le_u16(&bytes, body + 14)?;
            if format != 1 || channels != 1 || bits != 16 {
                return Err(format!("{}: need mono 16-bit PCM", path.display()).into());
            }
            rate = Some(le_u32(&bytes, body + 4)?);
        } else if id == b"data" {
            let rate = rate.ok_or("WAV data before format")?;
            // A streamed file may leave the size unset; read to the end.
            let end = (body + size).min(bytes.len());
            let samples = bytes[body..end]
                .chunks_exact(2)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            return Ok(Wav { rate, samples });
        }
        at = body + size + (size & 1);
    }
    Err(format!("{}: no audio data", path.display()).into())
}

/// How a stream is encoded.
#[derive(Clone, Copy, Debug)]
pub struct EncodeSettings {
    pub rate: u32,
    pub bitrate: i32,
    pub complexity: i32,
    /// Forces the fullband mode a browser uses for a real microphone, even
    /// when the test signal has little energy above 8 kHz.
    pub force_fullband: bool,
}

/// Builds an encoder with the given settings.
pub fn encoder(settings: EncodeSettings) -> Result<opus::Encoder> {
    let mut encoder =
        opus::Encoder::new(settings.rate, opus::Channels::Mono, opus::Application::Voip)?;
    encoder.set_bitrate(opus::Bitrate::Bits(settings.bitrate))?;
    encoder.set_complexity(settings.complexity)?;
    if settings.force_fullband {
        encoder.set_bandwidth(opus::Bandwidth::Fullband)?;
    }
    Ok(encoder)
}

/// Encodes `pcm` into 20 ms packets. A trailing partial frame is dropped.
pub fn encode_packets(pcm: &[i16], settings: EncodeSettings) -> Result<Vec<Box<[u8]>>> {
    let mut encoder = encoder(settings)?;
    let frame = (settings.rate / 50) as usize;
    let mut buffer = vec![0_u8; 1500];
    let mut packets = Vec::with_capacity(pcm.len() / frame);
    for chunk in pcm.chunks_exact(frame) {
        let len = encoder.encode(chunk, &mut buffer)?;
        packets.push(buffer[..len].to_vec().into_boxed_slice());
    }
    Ok(packets)
}
