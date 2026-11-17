//! Datagrams on a TCP connection (RFC 4571): each is preceded by its
//! length, in two bytes.
//!
//! It is how a call gets through a network that lets no UDP out: the
//! far end connects, and everything the call would send as datagrams is
//! written on the connection instead.

use core::task::{Context, Poll};
use std::io::ErrorKind;
use std::net::SocketAddr;

use bytes::{Buf, Bytes, BytesMut};
use tokio::net::TcpStream;

/// How much may wait for a connection that is behind. A datagram that
/// finds more waiting is dropped, as a network would drop it: audio
/// that arrives late is of no use.
const BEHIND: usize = 256 * 1024;

/// How much is read at once.
const AT_ONCE: usize = 4096;

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
#[derive(Debug)]
pub(crate) struct Framed {
    stream: TcpStream,
    peer: SocketAddr,
    read: BytesMut,
    /// What is still to be written.
    unsent: BytesMut,
    over: bool,
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
            over: false,
        }
    }

    /// Where the connection comes from, or goes to.
    pub(crate) const fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// The next datagram. `None` once the connection is over.
    pub(crate) fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Bytes>> {
        let mut piece = [0_u8; AT_ONCE];
        loop {
            if let Some(frame) = next_frame(&mut self.read) {
                return Poll::Ready(Some(frame));
            }
            if self.over {
                return Poll::Ready(None);
            }
            match self.stream.try_read(&mut piece) {
                Ok(0) => self.over = true,
                Ok(read) => self
                    .read
                    .extend_from_slice(piece.get(..read).unwrap_or(&[])),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    match self.stream.poll_read_ready(cx) {
                        Poll::Ready(Ok(())) => {}
                        Poll::Ready(Err(_)) => self.over = true,
                        Poll::Pending => return Poll::Pending,
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => self.over = true,
            }
        }
    }

    /// Sends a datagram: now, as far as the connection takes it, and
    /// the rest when it has room ([`poll_flush`](Self::poll_flush)).
    pub(crate) fn send(&mut self, datagram: &[u8]) {
        if self.over || self.unsent.len() > BEHIND {
            return;
        }
        put_frame(datagram, &mut self.unsent);
        self.write();
    }

    fn write(&mut self) {
        while !self.unsent.is_empty() && !self.over {
            match self.stream.try_write(&self.unsent) {
                Ok(0) => self.over = true,
                Ok(written) => self.unsent.advance(written),
                Err(error) if error.kind() == ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => self.over = true,
            }
        }
    }

    /// Writes what waits, and asks to be woken when the connection has
    /// room for what still does.
    pub(crate) fn poll_flush(&mut self, cx: &mut Context<'_>) {
        loop {
            self.write();
            if self.unsent.is_empty() || self.over {
                return;
            }
            match self.stream.poll_write_ready(cx) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(_)) => self.over = true,
                Poll::Pending => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;

    use super::{next_frame, put_frame};

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
