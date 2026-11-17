//! Redundant audio (RFC 2198): a packet that carries the frames before
//! its own beside it.
//!
//! ```text
//! for each redundant block, the oldest first:  1 | 7: payload type | 14: how far back | 10: length
//! for the block of the packet itself:          0 | 7: payload type
//! then the blocks, in that order; the last has no length and takes the rest.
//! ```
//!
//! How far back a block is, is counted in units of the RTP clock from
//! the time of the packet.

/// The furthest back a block can say it is.
const FURTHEST: u32 = (1 << 14) - 1;

/// The longest a redundant block can say it is.
const LONGEST: usize = (1 << 10) - 1;

/// One block of a packet of redundant audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Block<'a> {
    /// The payload type its audio is written as.
    pub(crate) kind: u8,
    /// How far before the time of the packet its audio begins. Nought
    /// for the packet's own.
    pub(crate) back: u32,
    /// The audio.
    pub(crate) audio: &'a [u8],
}

/// The blocks of a packet of redundant audio, the oldest first and the
/// packet's own last.
#[derive(Clone, Debug)]
pub(crate) struct Blocks<'a> {
    heads: &'a [u8],
    audio: &'a [u8],
}

impl<'a> Iterator for Blocks<'a> {
    type Item = Block<'a>;

    fn next(&mut self) -> Option<Block<'a>> {
        let (first, rest) = self.heads.split_first()?;
        let kind = first & 0x7f;
        if first & 0x80 == 0 {
            self.heads = rest;
            let audio = core::mem::take(&mut self.audio);
            return Some(Block {
                kind,
                back: 0,
                audio,
            });
        }
        let (&[high, low, length], rest) = rest.split_first_chunk::<3>()?;
        self.heads = rest;
        let back = (u32::from(high) << 6) | (u32::from(low) >> 2);
        let length = (usize::from(low & 0x03) << 8) | usize::from(length);
        let (audio, rest) = self.audio.split_at_checked(length)?;
        self.audio = rest;
        Some(Block { kind, back, audio })
    }
}

/// Reads a packet of redundant audio. `None` for one that is cut short
/// or says more than it holds.
fn unpack(payload: &[u8]) -> Option<Blocks<'_>> {
    // Where the headers end, and how much the blocks before the last
    // say they hold.
    let (mut at, mut held) = (0_usize, 0_usize);
    loop {
        let first = *payload.get(at)?;
        if first & 0x80 == 0 {
            at += 1;
            break;
        }
        let low = *payload.get(at + 2)?;
        let length = *payload.get(at + 3)?;
        held += (usize::from(low & 0x03) << 8) | usize::from(length);
        at += 4;
    }
    let (heads, audio) = payload.split_at_checked(at)?;
    (audio.len() >= held).then_some(Blocks { heads, audio })
}

/// The frames of a packet of redundant audio that are written as
/// `kind`, which is the audio's, each with how far back it is, the
/// oldest first. What else was put beside them is passed over: a
/// payload of this kind may carry what is no audio, and a frame of
/// noise or a key is not to be played as one. `None` for a packet that
/// is cut short or says more than it holds.
pub(crate) fn frames(payload: &[u8], kind: u8) -> Option<impl Iterator<Item = Block<'_>>> {
    Some(unpack(payload)?.filter(move |block| block.kind == kind))
}

/// Writes a packet of redundant audio into `out`: `own`, written as
/// `kind`, with `before` beside it, each with how far back it is, the
/// oldest first. A block that is too long or too far back to be said is
/// left out.
pub(crate) fn pack(kind: u8, before: &[(u32, &[u8])], own: &[u8], out: &mut Vec<u8>) {
    out.clear();
    let said = |(back, audio): &&(u32, &[u8])| *back <= FURTHEST && audio.len() <= LONGEST;
    for (back, audio) in before.iter().filter(said) {
        let [.., back_high, back_low] = (back << 2).to_be_bytes();
        let [.., length_high, length_low] = audio.len().to_be_bytes();
        out.extend_from_slice(&[
            0x80 | (kind & 0x7f),
            back_high,
            back_low | (length_high & 0x03),
            length_low,
        ]);
    }
    out.push(kind & 0x7f);
    for (_, audio) in before.iter().filter(said) {
        out.extend_from_slice(audio);
    }
    out.extend_from_slice(own);
}

#[cfg(test)]
mod tests {
    use super::{Block, frames, pack, unpack};

    fn blocks(payload: &[u8]) -> Option<Vec<Block<'_>>> {
        Some(unpack(payload)?.collect())
    }

    #[test]
    fn what_is_packed_is_what_is_read() {
        let (older, old, own) = ([1_u8; 40], [2_u8; 300], [3_u8; 77]);
        let mut packed = Vec::new();
        pack(111, &[(1_920, &older), (960, &old)], &own, &mut packed);
        assert_eq!(packed.len(), 4 + 4 + 1 + 40 + 300 + 77);
        let read = blocks(&packed).unwrap();
        let block = |back, audio| Block {
            kind: 111,
            back,
            audio,
        };
        assert_eq!(
            read,
            [
                block(1_920, &older[..]),
                block(960, &old[..]),
                block(0, &own[..])
            ]
        );
        // The example of the RFC's own layout, by hand: a block 960
        // back and 2 long, then the packet's own.
        let by_hand = [0x80 | 0x6f, 0x0f, 0x00, 0x02, 111, 7, 7, 9, 9, 9];
        assert_eq!(
            blocks(&by_hand).unwrap(),
            [block(960, &[7, 7][..]), block(0, &[9, 9, 9][..])]
        );
        // A packet that is reused holds only what was packed last.
        pack(96, &[], &[5, 5], &mut packed);
        assert_eq!(packed, [96, 5, 5]);
        let alone = blocks(&packed).unwrap();
        assert_eq!(
            (alone.len(), alone[0].kind, alone[0].audio),
            (1, 96, &[5, 5][..])
        );
    }

    #[test]
    fn the_edges_of_what_a_block_can_say() {
        let own = [9_u8; 3];
        let longest = [4_u8; 1_023];
        let mut packed = Vec::new();
        pack(111, &[(16_383, &longest)], &own, &mut packed);
        let read = blocks(&packed).unwrap();
        assert_eq!((read[0].back, read[0].audio.len()), (16_383, 1_023));
        assert_eq!(read[1].audio, own);
        // One that is too long, or too far back, is left out, and the
        // rest is still a packet.
        let too_long = [4_u8; 1_024];
        pack(111, &[(960, &too_long), (480, &own)], &own, &mut packed);
        let read = blocks(&packed).unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!((read[0].back, read[0].audio), (480, &own[..]));
        pack(111, &[(16_384, &own)], &own, &mut packed);
        assert_eq!(blocks(&packed).unwrap().len(), 1);
        // A block may be empty: a sender that has nothing for a place
        // says so.
        pack(111, &[(960, &[])], &own, &mut packed);
        let read = blocks(&packed).unwrap();
        assert_eq!((read.len(), read[0].audio.len()), (2, 0));
    }

    #[test]
    fn what_is_beside_the_audio_and_is_none_is_passed_over() {
        // Comfort noise from 20 ms back, written as 13, beside a frame
        // of audio: a browser may put either into a payload of this
        // kind.
        let payload = [0x80 | 0x0d, 0x0f, 0x00, 2, 111, 9, 9, 7, 7, 7];
        assert_eq!(blocks(&payload).unwrap().len(), 2);
        let audio: Vec<Block<'_>> = frames(&payload, 111).unwrap().collect();
        let own = Block {
            kind: 111,
            back: 0,
            audio: &[7, 7, 7],
        };
        assert_eq!(audio, [own]);
        // And the other way about.
        let payload = [0x80 | 0x6f, 0x0f, 0x00, 2, 13, 9, 9, 7];
        let audio: Vec<Block<'_>> = frames(&payload, 111).unwrap().collect();
        let before = Block {
            kind: 111,
            back: 960,
            audio: &[9, 9],
        };
        assert_eq!(audio, [before]);
        assert!(frames(&[0x80 | 0x6f, 0x0f, 0x00, 200, 111, 1, 2], 111).is_none());
    }

    #[test]
    fn a_packet_that_says_more_than_it_holds_is_none() {
        for cut in [
            &[][..],
            // A header that is cut short, and one with nothing after.
            &[0x80 | 0x6f, 0x0f, 0x00],
            &[0x80 | 0x6f, 0x0f, 0x00, 0x02],
            // A block of 200 where there are 2.
            &[0x80 | 0x6f, 0x0f, 0x00, 200, 111, 1, 2],
        ] {
            assert!(unpack(cut).is_none(), "{cut:?}");
        }
        // A packet of nothing but its own header holds no audio.
        let empty = blocks(&[111]).unwrap();
        assert_eq!((empty.len(), empty[0].audio.len()), (1, 0));
    }
}
