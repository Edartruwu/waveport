//! An end of a call with its sockets and its clock.
//!
//! [`Driven`] is what runs an end ([`Core`]): it gives it what arrives
//! and the time, sends what it wants sent, and sleeps until it wants
//! to be asked again. Where datagrams come from and go to is a
//! [`Wire`]'s to know: the socket every call of a server shares, or
//! the one a client has to itself.

use core::future::poll_fn;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Instant;

use bytes::Bytes;
use str0m::net::{Protocol, Transmit};
use tokio::time::Sleep;

use crate::engine::{Core, Stepped};

/// How many datagrams an end is given before whoever else waits for
/// the thread gets a turn.
const AT_ONCE: usize = 64;

/// What time it is. It is the clock tokio keeps, so that a test that
/// moves that clock moves the calls with it.
pub(crate) fn now() -> Instant {
    tokio::time::Instant::now().into_std()
}

/// A datagram that arrived for a call.
#[derive(Debug)]
pub(crate) struct Arrived {
    pub(crate) protocol: Protocol,
    pub(crate) from: SocketAddr,
    /// Where it arrived, as the far end was told to reach it.
    pub(crate) to: SocketAddr,
    pub(crate) datagram: Bytes,
}

/// Where the datagrams of one end of a call come from and go to.
pub(crate) trait Wire {
    /// The next datagram that arrived. It asks to be woken when one
    /// does.
    fn poll_arrived(&mut self, cx: &mut Context<'_>) -> Poll<Arrived>;

    /// What the call said of the datagram that arrived last, from
    /// `from`: whether it is the call's.
    fn judged(&mut self, from: SocketAddr, taken: bool) {
        let _ = (from, taken);
    }

    /// Sends a datagram, or drops it as a network might.
    fn send(&mut self, transmit: &Transmit);

    /// Sends what could not be sent before, and asks to be woken when
    /// more of it can be.
    fn poll_flush(&mut self, cx: &mut Context<'_>) {
        let _ = cx;
    }
}

/// An end of a call, run.
pub(crate) struct Driven<C: Core, W> {
    pub(crate) core: C,
    pub(crate) wire: W,
    /// What happened while only what was to be sent was asked for
    /// ([`flush`](Self::flush)).
    kept: VecDeque<C::Told>,
    /// Until the end wants to be asked again. It is made when it is
    /// first needed, on the thread that runs the end by then.
    sleep: Option<Pin<Box<Sleep>>>,
}

impl<C: Core, W: Wire> Driven<C, W> {
    pub(crate) const fn new(core: C, wire: W) -> Self {
        Self {
            core,
            wire,
            kept: VecDeque::new(),
            sleep: None,
        }
    }

    /// Runs the end until something happened on it.
    pub(crate) fn poll_told(&mut self, cx: &mut Context<'_>) -> Poll<C::Told> {
        if let Some(told) = self.kept.pop_front() {
            return Poll::Ready(told);
        }
        let mut taken = 0;
        loop {
            let now = now();
            let until = match self.core.step(now) {
                Stepped::Send(transmit) => {
                    self.wire.send(&transmit);
                    continue;
                }
                Stepped::Told(told) => return Poll::Ready(told),
                Stepped::Wait(until) => until,
            };
            self.wire.poll_flush(cx);
            if taken >= AT_ONCE {
                // There may be more. Others go first.
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            if let Poll::Ready(arrived) = self.wire.poll_arrived(cx) {
                taken += 1;
                let Arrived {
                    protocol,
                    from,
                    to,
                    datagram,
                } = arrived;
                let taken = self.core.receive(now, protocol, from, to, &datagram);
                self.wire.judged(from, taken);
                continue;
            }
            let until = tokio::time::Instant::from_std(until);
            let sleep = match &mut self.sleep {
                Some(sleep) => {
                    if sleep.deadline() != until {
                        sleep.as_mut().reset(until);
                    }
                    sleep
                }
                None => self.sleep.insert(Box::pin(tokio::time::sleep_until(until))),
            };
            if sleep.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
        }
    }

    /// The next thing that happened on the end.
    pub(crate) async fn told(&mut self) -> C::Told {
        poll_fn(|cx| self.poll_told(cx)).await
    }

    /// Takes in what has arrived for the end, for an end that nobody
    /// has run for a while, and waits for nothing. What the end says
    /// happened is kept for whoever asks next.
    pub(crate) fn catch_up(&mut self, cx: &mut Context<'_>) {
        let now = now();
        for _ in 0..AT_ONCE {
            let Poll::Ready(arrived) = self.wire.poll_arrived(cx) else {
                break;
            };
            let Arrived {
                protocol,
                from,
                to,
                datagram,
            } = arrived;
            let taken = self.core.receive(now, protocol, from, to, &datagram);
            self.wire.judged(from, taken);
        }
        self.flush();
    }

    /// Sends what the end wants sent now, and waits for nothing: for
    /// what was just said to be on its way before anything else is
    /// done. What the end says happened meanwhile is kept for whoever
    /// asks next.
    pub(crate) fn flush(&mut self) {
        let now = now();
        loop {
            match self.core.step(now) {
                Stepped::Send(transmit) => self.wire.send(&transmit),
                Stepped::Told(told) => self.kept.push_back(told),
                Stepped::Wait(_) => return,
            }
        }
    }
}
