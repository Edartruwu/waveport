//! The journal in memory, held to what every journal is held to.

use bytes::Bytes;
use waveport_ports::journal::{Batch, Journal, JournalError, Lease, Seq, SessionId, Snapshot};
use waveport_store_memory::MemoryJournal;
use waveport_testkit::conformance;

#[tokio::test(start_paused = true)]
async fn it_keeps_the_rules_of_a_journal() {
    let journal = MemoryJournal::new();
    conformance::journal_session(&journal, SessionId(7))
        .await
        .unwrap();
    // Another session in the same journal is another session.
    conformance::journal_session(&journal, SessionId(8))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_session_nobody_has_begun_cannot_be_written_to() {
    let journal = MemoryJournal::new();
    let batch = Batch {
        first: Seq(1),
        records: vec![Bytes::from_static(b"a record")],
    };
    assert_eq!(
        journal.append(SessionId(1), Lease(1), batch).await,
        Err(JournalError::Fenced)
    );
    assert_eq!(journal.held(SessionId(1)), 0);
}

#[tokio::test]
async fn records_a_snapshot_covers_are_let_go_of() {
    let journal = MemoryJournal::new();
    let session = SessionId(3);
    let lease = journal.acquire(session).await.unwrap().lease;
    let records = (0..10).map(|n| Bytes::from(vec![n])).collect();
    let batch = Batch {
        first: Seq(1),
        records,
    };
    assert_eq!(journal.append(session, lease, batch).await, Ok(Seq(10)));
    assert_eq!(journal.held(session), 10);

    let snapshot = |upto: u64| Snapshot {
        upto: Seq(upto),
        state: Bytes::from(vec![0xAA; 4]),
    };
    journal.snapshot(session, lease, snapshot(6)).await.unwrap();
    assert_eq!(journal.held(session), 4);
    // An older state than the one held is not taken in its place.
    assert_eq!(
        journal.snapshot(session, lease, snapshot(4)).await,
        Err(JournalError::OutOfOrder { expected: Seq(11) })
    );
    // The state as of the last record leaves no record to keep.
    journal
        .snapshot(session, lease, snapshot(10))
        .await
        .unwrap();
    assert_eq!(journal.held(session), 0);

    let again = journal.acquire(session).await.unwrap();
    assert_eq!(again.snapshot, Some(snapshot(10)));
    assert_eq!(again.tail, Vec::<Bytes>::new());
    assert_eq!(again.next, Seq(11));
}
