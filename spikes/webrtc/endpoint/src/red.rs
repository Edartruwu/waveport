//! RFC 2198 redundant audio: packing and unpacking of the payload.
//!
//! ```text
//!  for every redundant block, oldest first:   1 F=1 | 7 block PT | 14 timestamp offset | 10 length
//!  for the primary block:                     1 F=0 | 7 block PT
//!  then the blocks' data in the same order, the primary last and unlengthed.
//! ```

/// One block of a RED payload: how far back in RTP time it is, and its data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block<'a> {
    pub pt: u8,
    /// RTP time before the packet's own timestamp; 0 for the primary.
    pub offset: u32,
    pub data: &'a [u8],
}

/// Unpacks a RED payload. `None` when it is malformed.
pub fn unpack(payload: &[u8]) -> Option<Vec<Block<'_>>> {
    let mut heads: Vec<(u8, u32, Option<usize>)> = Vec::new();
    let mut i = 0;
    loop {
        let b = *payload.get(i)?;
        if b & 0x80 == 0 {
            heads.push((b & 0x7f, 0, None));
            i += 1;
            break;
        }
        let h = payload.get(i..i + 4)?;
        let offset = (u32::from(h[1]) << 6) | (u32::from(h[2]) >> 2);
        let len = ((usize::from(h[2]) & 0x03) << 8) | usize::from(h[3]);
        heads.push((b & 0x7f, offset, Some(len)));
        i += 4;
    }
    let mut out = Vec::with_capacity(heads.len());
    for (pt, offset, len) in heads {
        let data = match len {
            Some(len) => {
                let d = payload.get(i..i + len)?;
                i += len;
                d
            }
            None => payload.get(i..)?,
        };
        out.push(Block { pt, offset, data });
    }
    Some(out)
}

/// Packs a primary block with redundant ones (`(offset, data)`, oldest first).
/// A redundant block over 1023 bytes or further back than 16383 is left out.
pub fn pack(pt: u8, redundant: &[(u32, &[u8])], primary: &[u8]) -> Vec<u8> {
    let kept: Vec<&(u32, &[u8])> =
        redundant.iter().filter(|(off, d)| *off < (1 << 14) && d.len() < (1 << 10)).collect();
    let mut out = Vec::with_capacity(1 + kept.len() * 4 + primary.len() + kept.iter().map(|r| r.1.len()).sum::<usize>());
    for (off, d) in &kept {
        out.push(0x80 | pt);
        out.push((off >> 6) as u8);
        out.push((((off & 0x3f) << 2) as u8) | ((d.len() >> 8) as u8 & 0x03));
        out.push(d.len() as u8);
    }
    out.push(pt & 0x7f);
    for (_, d) in &kept {
        out.extend_from_slice(d);
    }
    out.extend_from_slice(primary);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let a = [1u8; 40];
        let b = [2u8; 300];
        let p = [3u8; 77];
        let packed = pack(111, &[(1920, &a), (960, &b)], &p);
        let blocks = unpack(&packed).unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0], Block { pt: 111, offset: 1920, data: &a });
        assert_eq!(blocks[1], Block { pt: 111, offset: 960, data: &b });
        assert_eq!(blocks[2], Block { pt: 111, offset: 0, data: &p });
    }

    #[test]
    fn primary_only_and_garbage() {
        let packed = pack(111, &[], &[9, 9]);
        assert_eq!(packed, vec![111, 9, 9]);
        assert_eq!(unpack(&packed).unwrap().len(), 1);
        assert!(unpack(&[0x80 | 111, 0, 0]).is_none());
        assert!(unpack(&[0x80 | 111, 0, 0, 200, 111, 1, 2]).is_none());
    }
}
