//! Audio that arrives in pieces cut anywhere.

use bytes::{Bytes, BytesMut};
use waveport_ports::audio::PcmBytes;

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

    use super::PcmAssembler;

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
