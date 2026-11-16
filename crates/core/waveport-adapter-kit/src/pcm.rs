//! Audio as providers send and take it: in pieces cut anywhere, and
//! written out as text inside a message.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::{Bytes, BytesMut};
use waveport_ports::audio::PcmBytes;
use waveport_ports::error::PortError;

/// Writes samples at the end of `out` as providers want them inside a
/// JSON message: 16 bits each, low byte first, in base64.
pub fn write_base64(samples: &[i16], out: &mut String) {
    // A number of bytes that three divides, so that the padding of base64
    // can only come at the very end.
    const AT_ONCE: usize = 1_536;
    let mut bytes = [0_u8; AT_ONCE * 2];
    out.reserve(samples.len() * 8 / 3 + 4);
    for chunk in samples.chunks(AT_ONCE) {
        let (pairs, _) = bytes.as_chunks_mut::<2>();
        for (pair, sample) in pairs.iter_mut().zip(chunk) {
            *pair = sample.to_le_bytes();
        }
        let written = bytes.get(..chunk.len() * 2).unwrap_or_default();
        STANDARD.encode_string(written, out);
    }
}

/// Reads bytes a provider wrote out in base64 inside a message.
///
/// # Errors
///
/// The text is not base64. A provider that sends such a thing has sent
/// something this adapter does not understand, and the same request would
/// be answered the same.
pub fn read_base64(text: &str) -> Result<Bytes, PortError> {
    STANDARD
        .decode(text)
        .map(Bytes::from)
        .map_err(|error| PortError::permanent(format!("audio that is not base64: {error}")))
}

/// Puts whole samples together from a stream of bytes that may be cut in
/// the middle of one.
///
/// A provider that streams plain samples cuts its stream where its
/// buffers end. A sample is two bytes, so a piece can end with half of
/// one. Dropping that byte would shift every sample after it and turn the
/// rest of the response into noise, so it is kept for the next piece.
#[derive(Clone, Debug, Default)]
pub struct PcmAssembler {
    /// The first byte of a sample whose second has not arrived.
    half: Option<u8>,
}

impl PcmAssembler {
    /// An assembler at the start of a stream.
    #[must_use]
    pub const fn new() -> Self {
        Self { half: None }
    }

    /// Takes the next piece and gives the whole samples there are now.
    /// A piece that begins and ends on a sample is handed on as it is,
    /// without a copy.
    pub fn push(&mut self, piece: Bytes) -> Option<PcmBytes> {
        let whole = match self.half.take() {
            None if piece.len().is_multiple_of(2) => piece,
            None => {
                let mut piece = piece;
                self.half = piece.last().copied();
                piece.truncate(piece.len() - 1);
                piece
            }
            Some(half) => {
                let mut joined = BytesMut::with_capacity(piece.len() + 1);
                joined.extend_from_slice(&[half]);
                joined.extend_from_slice(&piece);
                if !joined.len().is_multiple_of(2) {
                    self.half = joined.last().copied();
                    joined.truncate(joined.len() - 1);
                }
                joined.freeze()
            }
        };
        if whole.is_empty() {
            return None;
        }
        PcmBytes::new(whole).ok()
    }

    /// Whether half a sample is waiting for its other half. At the end of
    /// a stream that half is dropped.
    #[must_use]
    pub const fn has_half(&self) -> bool {
        self.half.is_some()
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use waveport_ports::audio::PcmBytes;
    use waveport_ports::error::RetryClass;

    use super::{PcmAssembler, read_base64, write_base64};

    #[test]
    fn samples_written_as_text_are_read_back_the_same() {
        for count in [0_usize, 1, 2, 3, 480, 1_535, 1_536, 1_537, 4_800] {
            let samples: Vec<i16> = (0..count)
                .map(|n| i16::try_from(n * 37 % 60_000).unwrap_or(i16::MAX) - 30_000)
                .collect();
            let mut text = String::from("audio:");
            write_base64(&samples, &mut text);
            let text = text.strip_prefix("audio:").unwrap();
            // Padding comes only at the end, as one piece of base64 has it.
            assert!(!text.trim_end_matches('=').contains('='), "{count}");
            let pcm = PcmBytes::new(read_base64(text).unwrap()).unwrap();
            assert_eq!(pcm.samples().collect::<Vec<_>>(), samples, "{count}");
        }
        // Known values: 1 and -2, low byte first.
        let mut text = String::new();
        write_base64(&[1, -2], &mut text);
        assert_eq!(text, "AQD+/w==");
    }

    #[test]
    fn what_is_not_base64_is_refused_for_good() {
        let error = read_base64("not base64!").unwrap_err();
        assert_eq!(error.class, RetryClass::Permanent);
    }

    #[test]
    fn whole_pieces_pass_as_they_are() {
        let mut assembler = PcmAssembler::new();
        let piece = Bytes::from_static(&[1, 0, 2, 0]);
        let pcm = assembler.push(piece.clone()).unwrap();
        assert_eq!(pcm.samples().collect::<Vec<_>>(), [1, 2]);
        // The same memory, not a copy.
        assert_eq!(pcm.as_bytes().as_ptr(), piece.as_ptr());
        assert!(!assembler.has_half());
        assert_eq!(assembler.push(Bytes::new()), None);
    }

    #[test]
    fn however_a_stream_is_cut_the_samples_are_the_same() {
        let samples: Vec<i16> = (0..50).map(|n| n * 613 - 15_000).collect();
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        for cut in [1, 2, 3, 5, 7, 99, 100] {
            let mut assembler = PcmAssembler::new();
            let mut heard = Vec::new();
            for piece in bytes.chunks(cut) {
                if let Some(pcm) = assembler.push(Bytes::copy_from_slice(piece)) {
                    heard.extend(pcm.samples());
                }
            }
            assert_eq!(heard, samples, "cut every {cut} bytes");
            assert!(!assembler.has_half());
        }
    }

    #[test]
    fn half_a_sample_waits_for_the_other_half() {
        let mut assembler = PcmAssembler::new();
        assert_eq!(assembler.push(Bytes::from_static(&[0x34])), None);
        assert!(assembler.has_half());
        let pcm = assembler.push(Bytes::from_static(&[0x12])).unwrap();
        assert_eq!(pcm.samples().collect::<Vec<_>>(), [0x1234]);
    }
}
