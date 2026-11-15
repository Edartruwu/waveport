//! What the journal's file holds, and how it is written there.
//!
//! Three tables. `heads` has one row for every session the journal knows:
//! who owns it, the number its next record gets, the record its state is
//! as of, and the hash of its resume token. `batches` has the records, a
//! row for every group that was appended together, under the session and
//! the number of the group's first record. `snapshots` has the newest
//! state of each session.
//!
//! A fourth table, `meta`, says which version of this layout the file was
//! written in, and how many commits have changed the file. A file of a
//! later version is not opened. The count is how the journal tells, after
//! a commit failed, whether the file has it all the same.
//!
//! Numbers are written little-endian. Every row a later build might want
//! to change begins with a byte that says how it is written.

use bytes::Bytes;
use redb::TableDefinition;
use waveport_ports::error::PortError;
use waveport_ports::journal::Lease;

/// The version of the layout this build writes.
pub(crate) const FORMAT: u64 = 1;

/// The row of `meta` that holds the version of the layout.
pub(crate) const FORMAT_KEY: &str = "format";

/// The row of `meta` that holds how many commits have changed the file.
pub(crate) const COMMITS_KEY: &str = "commits";

pub(crate) const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
pub(crate) const HEADS: TableDefinition<u128, &[u8]> = TableDefinition::new("heads");
pub(crate) const BATCHES: TableDefinition<(u128, u64), &[u8]> = TableDefinition::new("batches");
pub(crate) const STATES: TableDefinition<u128, &[u8]> = TableDefinition::new("snapshots");

/// How a head is written: the only way there is so far.
const HEAD_V1: u8 = 1;

/// The length of a head: the byte that says how it is written, the
/// lease, the next number, whether there is a state and the record it is
/// as of, whether there is a token and its hash.
const HEAD_LEN: usize = 1 + 8 + 8 + 1 + 8 + 1 + 32;

/// What the journal knows of a session beside its records.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Head {
    /// The lease of the session's owner. 0 for a session nobody has
    /// taken yet.
    pub lease: Lease,
    /// The number the session's next record gets. Never 0.
    pub next: u64,
    /// The record the stored state is as of, if a state is stored.
    pub state: Option<u64>,
    /// The hash of the session's resume token, if one was issued.
    pub token: Option<[u8; 32]>,
}

impl Head {
    /// The head of a session nothing is stored of.
    pub(crate) const fn new() -> Self {
        Self {
            lease: Lease(0),
            next: 1,
            state: None,
            token: None,
        }
    }

    pub(crate) fn write(&self) -> [u8; HEAD_LEN] {
        let mut out = [0; HEAD_LEN];
        let mut at = 0;
        let mut put = |bytes: &[u8]| {
            if let Some(room) = out.get_mut(at..at + bytes.len()) {
                room.copy_from_slice(bytes);
            }
            at += bytes.len();
        };
        put(&[HEAD_V1]);
        put(&self.lease.0.to_le_bytes());
        put(&self.next.to_le_bytes());
        put(&[u8::from(self.state.is_some())]);
        put(&self.state.unwrap_or(0).to_le_bytes());
        put(&[u8::from(self.token.is_some())]);
        put(&self.token.unwrap_or([0; 32]));
        out
    }

    pub(crate) fn read(written: &[u8]) -> Result<Self, PortError> {
        let mut rest = written;
        let version = take::<1>(&mut rest).ok_or_else(|| unreadable("a head is cut short"))?;
        if version != [HEAD_V1] {
            return Err(unreadable(
                "a head is written in a way this build does not know",
            ));
        }
        let mut number = || take::<8>(&mut rest).map(u64::from_le_bytes);
        let (Some(lease), Some(next)) = (number(), number()) else {
            return Err(unreadable("a head is cut short"));
        };
        let has_state = take::<1>(&mut rest);
        let state = take::<8>(&mut rest).map(u64::from_le_bytes);
        let has_token = take::<1>(&mut rest);
        let token = take::<32>(&mut rest);
        let (Some([has_state]), Some(state), Some([has_token]), Some(token)) =
            (has_state, state, has_token, token)
        else {
            return Err(unreadable("a head is cut short"));
        };
        if next == 0 || has_state > 1 || has_token > 1 {
            return Err(unreadable("a head says what no head can say"));
        }
        Ok(Self {
            lease: Lease(lease),
            next,
            state: (has_state == 1).then_some(state),
            token: (has_token == 1).then_some(token),
        })
    }
}

/// Takes the first `N` bytes off `rest`.
fn take<const N: usize>(rest: &mut &[u8]) -> Option<[u8; N]> {
    let (taken, left) = rest.split_first_chunk::<N>()?;
    *rest = left;
    Some(*taken)
}

/// The failure of a journal whose file holds what cannot be read. Trying
/// again does not mend it.
pub(crate) fn unreadable(what: &str) -> PortError {
    PortError::permanent(format!("the journal cannot be read: {what}"))
}

/// Writes the records of a group as one row: how many there are, and
/// then each with its length before it.
pub(crate) fn write_batch(records: &[Bytes]) -> Result<Vec<u8>, PortError> {
    let too_large = || PortError::permanent("a record is too large for the journal");
    let count = u32::try_from(records.len()).map_err(|_| too_large())?;
    let bytes: usize = records.iter().map(|record| 4 + record.len()).sum();
    let mut out = Vec::with_capacity(4 + bytes);
    out.extend_from_slice(&count.to_le_bytes());
    for record in records {
        let len = u32::try_from(record.len()).map_err(|_| too_large())?;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(record);
    }
    Ok(out)
}

/// How many records a row of `batches` holds.
pub(crate) fn batch_len(mut row: &[u8]) -> Result<u64, PortError> {
    take::<4>(&mut row)
        .map(|count| u64::from(u32::from_le_bytes(count)))
        .ok_or_else(|| unreadable("a group of records is cut short"))
}

/// Reads a row of `batches` back, and gives each record to `each` with
/// its place in the group, from 0.
pub(crate) fn read_batch(
    mut row: &[u8],
    mut each: impl FnMut(u64, &[u8]),
) -> Result<(), PortError> {
    let cut_short = || unreadable("a group of records is cut short");
    let count = take::<4>(&mut row)
        .map(u32::from_le_bytes)
        .ok_or_else(cut_short)?;
    for place in 0..count {
        let len = take::<4>(&mut row)
            .map(u32::from_le_bytes)
            .ok_or_else(cut_short)?;
        let len = usize::try_from(len).map_err(|_| cut_short())?;
        let (record, rest) = row.split_at_checked(len).ok_or_else(cut_short)?;
        each(u64::from(place), record);
        row = rest;
    }
    if !row.is_empty() {
        return Err(unreadable("a group of records has more in it than it says"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use waveport_ports::journal::Lease;

    use super::{Head, batch_len, read_batch, write_batch};

    #[test]
    fn a_head_comes_back_as_it_was_written() {
        let heads = [
            Head::new(),
            Head {
                lease: Lease(u64::MAX),
                next: 7,
                state: Some(0),
                token: None,
            },
            Head {
                lease: Lease(3),
                next: u64::MAX,
                state: Some(41),
                token: Some([0xA5; 32]),
            },
            Head {
                lease: Lease(1),
                next: 1,
                state: None,
                token: Some([0; 32]),
            },
        ];
        for head in heads {
            assert_eq!(Head::read(&head.write()).unwrap(), head);
        }
        assert_eq!(Head::new().next, 1);
    }

    #[test]
    fn a_head_that_is_not_one_is_refused() {
        let good = Head {
            lease: Lease(2),
            next: 9,
            state: Some(4),
            token: Some([7; 32]),
        }
        .write();
        // Cut anywhere, it is not a head.
        for len in 0..good.len() {
            assert!(Head::read(&good[..len]).is_err(), "{len} bytes");
        }
        let changed = |at: usize, to: u8| {
            let mut bad = good;
            bad[at] = to;
            Head::read(&bad)
        };
        // Written another way, a record numbered 0, and flags that are
        // neither yes nor no.
        assert!(changed(0, 2).is_err());
        assert!(changed(17, 2).is_err());
        assert!(changed(26, 9).is_err());
        let mut no_next = good;
        no_next[9..17].fill(0);
        assert!(Head::read(&no_next).is_err());
        let failure = changed(0, 2).unwrap_err();
        assert!(!failure.is_retryable(), "{failure}");
    }

    #[test]
    fn a_group_of_records_comes_back_as_it_was_written() {
        let groups: [&[&[u8]]; 4] = [
            &[],
            &[b""],
            &[b"one"],
            &[b"one", b"", b"three\n\0", &[0xFF; 300]],
        ];
        for group in groups {
            let records: Vec<Bytes> = group
                .iter()
                .map(|record| Bytes::from(record.to_vec()))
                .collect();
            let row = write_batch(&records).unwrap();
            assert_eq!(batch_len(&row).unwrap(), records.len() as u64);
            let mut back = Vec::new();
            read_batch(&row, |place, record| back.push((place, record.to_vec()))).unwrap();
            let expected: Vec<(u64, Vec<u8>)> = group
                .iter()
                .zip(0..)
                .map(|(record, place)| (place, record.to_vec()))
                .collect();
            assert_eq!(back, expected);
        }
    }

    #[test]
    fn a_group_that_is_cut_short_or_goes_on_is_refused() {
        let records = [Bytes::from_static(b"one"), Bytes::from_static(b"two")];
        let row = write_batch(&records).unwrap();
        for len in 0..row.len() {
            let cut = &row[..len];
            assert!(read_batch(cut, |_, _| {}).is_err(), "{len} bytes");
        }
        assert!(batch_len(&row[..3]).is_err());
        let mut longer = row;
        longer.push(0);
        assert!(read_batch(&longer, |_, _| {}).is_err());
    }
}
