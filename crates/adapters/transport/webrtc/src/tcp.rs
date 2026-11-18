//! Datagrams on a TCP connection (RFC 4571): each is preceded by its
//! length, in two bytes.
//!
//! It is how a call gets through a network that lets no UDP out: the
//! far end connects, and everything the call would send as datagrams is
//! written on the connection instead.
//!
//! The two directions of a connection end apart. What arrived on one
//! is given though nothing more can be sent on it, and one is let go
//! of by saying that nothing follows and taking what still comes.

use core::pin::Pin;
use core::task::{Context, Poll};
use std::io::ErrorKind;
use std::net::SocketAddr;

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::AsyncWrite;
use tokio::net::TcpStream;

/// How much may wait for a connection that is behind. A datagram that
/// finds more waiting is dropped, as a network would drop it: audio
/// that arrives late is of no use.
const BEHIND: usize = 256 * 1024;

/// How much is read at once.
const AT_ONCE: usize = 4096;

/// How many datagrams of a connection that is being let go of are
/// taken before others get the thread.
const LET_GO_AT_ONCE: usize = 64;

/// Takes the next whole datagram out of what was read.
pub(crate) fn next_frame(read: &mut BytesMut) -> Option<Bytes> {
    let [a, b] = *read.first_chunk::<2>()?;
    let length = usize::from(u16::from_be_bytes([a, b]));
    if read.len() < 2 + length {
        return None;
    }
    read.advance(2);
    Some(read.split_to(length).freeze())
}

/// Writes a datagram as a connection carries it. One that is longer
/// than two bytes can say is left out.
pub(crate) fn put_frame(datagram: &[u8], out: &mut BytesMut) {
    if let Ok(length) = u16::try_from(datagram.len()) {
        out.extend_from_slice(&length.to_be_bytes());
        out.extend_from_slice(datagram);
    }
}

/// A connection that carries datagrams.
///
/// Its two directions end apart. A far end that is gone is found out
/// by writing to it, and what it said before it went is still there to
/// be read: the last thing an end says is its goodbye, and whoever was
/// sending while it was said has not read it yet.
#[derive(Debug)]
pub(crate) struct Framed {
    stream: TcpStream,
    peer: SocketAddr,
    read: BytesMut,
    /// What is still to be written.
    unsent: BytesMut,
    /// Whether nothing more arrives on it.
    ended: bool,
    /// Whether nothing more is sent on it.
    shut: bool,
}

impl Framed {
    pub(crate) fn new(stream: TcpStream, peer: SocketAddr) -> Self {
        // A datagram is of no use later: it is not kept back for
        // another to share a segment with.
        let _ = stream.set_nodelay(true);
        Self {
            stream,
            peer,
            read: BytesMut::new(),
            unsent: BytesMut::new(),
            ended: false,
            shut: false,
        }
    }

    /// Where the connection comes from, or goes to.
    pub(crate) const fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// The next datagram. `None` once nothing more arrives: the far end
    /// closed the connection, or it broke. That nothing more can be
    /// sent on it does not end it: what arrived before is still given.
    pub(crate) fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Bytes>> {
        let mut piece = [0_u8; AT_ONCE];
        loop {
            if let Some(frame) = next_frame(&mut self.read) {
                return Poll::Ready(Some(frame));
            }
            if self.ended {
                return Poll::Ready(None);
            }
            match self.stream.try_read(&mut piece) {
                Ok(0) => self.ended = true,
                Ok(read) => self
                    .read
                    .extend_from_slice(piece.get(..read).unwrap_or(&[])),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    match self.stream.poll_read_ready(cx) {
                        Poll::Ready(Ok(())) => {}
                        Poll::Ready(Err(_)) => self.ended = true,
                        Poll::Pending => return Poll::Pending,
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => self.ended = true,
            }
        }
    }

    /// Sends a datagram: now, as far as the connection takes it, and
    /// the rest when it has room ([`poll_flush`](Self::poll_flush)).
    pub(crate) fn send(&mut self, datagram: &[u8]) {
        if self.shut || self.unsent.len() > BEHIND {
            return;
        }
        put_frame(datagram, &mut self.unsent);
        self.write();
    }

    /// Nothing more is sent: what waited is not going anywhere.
    fn stop(&mut self) {
        self.shut = true;
        self.unsent.clear();
    }

    fn write(&mut self) {
        while !self.unsent.is_empty() && !self.shut {
            match self.stream.try_write(&self.unsent) {
                Ok(0) => self.stop(),
                Ok(written) => self.unsent.advance(written),
                Err(error) if error.kind() == ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => self.stop(),
            }
        }
    }

    /// Writes what waits, and asks to be woken when the connection has
    /// room for what still does.
    pub(crate) fn poll_flush(&mut self, cx: &mut Context<'_>) {
        loop {
            self.write();
            if self.unsent.is_empty() || self.shut {
                return;
            }
            match self.stream.poll_write_ready(cx) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(_)) => self.stop(),
                Poll::Pending => return,
            }
        }
    }

    /// Lets go of the connection as one that is closed, and not as one
    /// that broke: writes what still waits, says that nothing follows
    /// it, and takes what the far end says until it has closed too.
    /// Ready once it has.
    ///
    /// A connection that is closed while something the far end said is
    /// unread is reset by the system, and a reset can take what was
    /// just said to the far end with it. The far end of a call sends
    /// all the time.
    pub(crate) fn poll_let_go(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if !self.shut {
            self.poll_flush(cx);
            if self.unsent.is_empty() && !self.shut {
                // It is done at once: nothing is waited for.
                let _ = Pin::new(&mut self.stream).poll_shutdown(cx);
                self.shut = true;
            }
        }
        for _ in 0..LET_GO_AT_ONCE {
            match self.poll_next(cx) {
                Poll::Ready(Some(_)) => {}
                Poll::Ready(None) => return Poll::Ready(()),
                Poll::Pending => return Poll::Pending,
            }
        }
        // There may be more. Others go first.
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// What the tests of this crate do with a connection.
#[cfg(test)]
pub(crate) mod testing {
    use core::future::poll_fn;
    use core::task::Poll;
    use core::time::Duration;

    use bytes::Bytes;
    use tokio::net::{TcpListener, TcpStream};

    use super::Framed;

    /// How long something that is to happen at once may take.
    const SOON: Duration = Duration::from_secs(10);

    /// The two ends of a connection.
    pub(crate) async fn connection() -> (Framed, Framed) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        let (near, far) = tokio::join!(TcpStream::connect(at), listener.accept());
        let (far, from) = far.unwrap();
        (Framed::new(near.unwrap(), at), Framed::new(far, from))
    }

    /// The next datagram that arrives at `end`.
    pub(crate) async fn next(end: &mut Framed) -> Option<Bytes> {
        let arrives = poll_fn(|cx| end.poll_next(cx));
        tokio::time::timeout(SOON, arrives).await.unwrap()
    }

    /// Sends `datagram` from `end`, and waits until the connection has
    /// taken all of it, or will take no more.
    pub(crate) async fn says(end: &mut Framed, datagram: &[u8]) {
        end.send(datagram);
        let taken = poll_fn(|cx| {
            end.poll_flush(cx);
            if end.unsent.is_empty() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        });
        tokio::time::timeout(SOON, taken).await.unwrap();
    }

    /// Waits until something that was sent to `end` is there to be
    /// read, and reads none of it.
    pub(crate) async fn has_something_unread(end: &Framed) {
        let there = end.stream.readable();
        tokio::time::timeout(SOON, there).await.unwrap().unwrap();
    }

    /// Whether `end` was refused what it sent, or has said that it
    /// sends no more.
    pub(crate) const fn sends_no_more(end: &Framed) -> bool {
        end.shut
    }
}

#[cfg(test)]
mod tests {
    use core::future::poll_fn;
    use core::task::Poll;
    use core::time::Duration;

    use bytes::BytesMut;

    use super::testing::{connection, has_something_unread, next, says, sends_no_more};
    use super::{next_frame, put_frame};

    /// How long something that is to happen at once may take.
    const SOON: Duration = Duration::from_secs(10);

    #[tokio::test]
    async fn what_arrived_is_given_though_nothing_more_is_sent() {
        let (mut near, mut far) = connection().await;
        says(&mut far, b"goodbye").await;
        drop(far);
        // The near end found that it can send no more, as an end does
        // that sends to one that is gone.
        near.stop();
        near.send(b"what the microphone hears");
        assert!(near.unsent.is_empty());
        // What the far end said before it went is not lost for that.
        assert_eq!(next(&mut near).await.unwrap(), b"goodbye"[..]);
        assert_eq!(next(&mut near).await, None);
    }

    /// On a system that keeps what arrived before a connection was
    /// reset. Not every system does.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_end_that_was_sending_as_the_other_left_still_reads_its_goodbye() {
        let (mut near, mut far) = connection().await;
        // The near end says something that the far end never reads, as
        // the far end of a call has: it sends all the time.
        says(&mut near, b"what the microphone hears").await;
        has_something_unread(&far).await;
        // The far end says goodbye and is gone. A connection that is
        // closed with something unread is reset.
        says(&mut far, b"goodbye").await;
        drop(far);
        // The near end finds out by sending.
        let refused = async {
            while !sends_no_more(&near) {
                near.send(b"more of it");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(SOON, refused).await.unwrap();
        assert_eq!(next(&mut near).await.unwrap(), b"goodbye"[..]);
        assert_eq!(next(&mut near).await, None);
    }

    #[tokio::test]
    async fn a_connection_that_is_let_go_of_ends_as_one_that_was_closed() {
        let (mut near, mut far) = connection().await;
        // The far end sends all the time, and what it sent is unread.
        says(&mut far, b"what the microphone hears").await;
        has_something_unread(&near).await;
        near.send(b"goodbye");
        // It is not over while the far end has not closed it too.
        let over = poll_fn(|cx| Poll::Ready(near.poll_let_go(cx))).await;
        assert!(over.is_pending());
        // The far end has what was said last, and then that nothing
        // follows it.
        assert_eq!(next(&mut far).await.unwrap(), b"goodbye"[..]);
        assert_eq!(next(&mut far).await, None);
        // Nothing more is sent on it.
        near.send(b"too late");
        assert!(near.unsent.is_empty());
        // What the far end goes on sending is taken: nothing refuses
        // it, however much it is.
        for _ in 0..200 {
            says(&mut far, &[7; 1_000]).await;
            let over = poll_fn(|cx| Poll::Ready(near.poll_let_go(cx))).await;
            assert!(over.is_pending());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        says(&mut far, b"and more").await;
        assert!(!sends_no_more(&far));
        // Once the far end closes, it is over.
        drop(far);
        let over = poll_fn(|cx| near.poll_let_go(cx));
        tokio::time::timeout(SOON, over).await.unwrap();
    }

    #[tokio::test]
    async fn what_still_waits_is_written_before_a_connection_is_let_go_of() {
        let (mut near, mut far) = connection().await;
        // More than the connection takes at once: the far end reads
        // nothing yet.
        let much = vec![3_u8; 60_000];
        let mut sent = 0;
        while near.unsent.is_empty() {
            near.send(&much);
            sent += 1;
            assert!(sent < 1_000, "the connection takes whatever it is given");
        }
        near.send(b"goodbye");
        let letting_go = async {
            poll_fn(|cx| near.poll_let_go(cx)).await;
        };
        let reading = async {
            let mut read = 0;
            let mut last = None;
            while let Some(datagram) = next(&mut far).await {
                read += 1;
                last = Some(datagram);
            }
            // All of it arrived, the goodbye last, before the end.
            assert_eq!(read, sent + 1);
            assert_eq!(last.unwrap(), b"goodbye"[..]);
            drop(far);
        };
        let both = async { tokio::join!(letting_go, reading) };
        tokio::time::timeout(SOON, both).await.unwrap();
    }

    #[test]
    fn datagrams_come_out_of_a_connection_as_they_went_in() {
        let mut wire = BytesMut::new();
        let datagrams: [&[u8]; 4] = [b"one", b"", &[7; 300], b"last"];
        for datagram in datagrams {
            put_frame(datagram, &mut wire);
        }
        assert_eq!(&wire[..5], [0, 3, b'o', b'n', b'e']);
        // One that two bytes cannot give the length of is not written.
        put_frame(&vec![0; 70_000], &mut wire);

        // Whatever pieces the connection delivers it in.
        for piece in [1, 2, 3, 7, 1_000] {
            let mut read = BytesMut::new();
            let mut heard = Vec::new();
            for bytes in wire.chunks(piece) {
                read.extend_from_slice(bytes);
                while let Some(frame) = next_frame(&mut read) {
                    heard.push(frame);
                }
            }
            assert_eq!(heard, datagrams, "{piece}");
            assert!(read.is_empty());
        }
        // Half of one is none yet.
        let mut read = BytesMut::from(&[0, 4, 1, 2][..]);
        assert_eq!(next_frame(&mut read), None);
        assert_eq!(read.len(), 4);
        read.extend_from_slice(&[3, 4, 0]);
        assert_eq!(next_frame(&mut read).unwrap(), [1, 2, 3, 4][..]);
        assert_eq!(next_frame(&mut read), None);
    }
}
