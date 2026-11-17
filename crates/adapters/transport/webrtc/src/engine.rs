//! `str0m`, held the way it has to be.
//!
//! Three things about it are easy to get wrong, and are kept in one
//! place:
//!
//! - It hands a packet of audio over at the next thing it is asked for
//!   after the packet was given to it, and has room for one. So it is
//!   asked for everything it has after every datagram: two datagrams
//!   given to it one after the other would cost the first its audio.
//! - What it wants sent it says in its own time, so everything that is
//!   written to it is followed by asking what is to be sent.
//! - Its release after 0.25.0 takes out panics that a far end can
//!   cause. Until then a call that panics is over, and its session with
//!   it, and nothing else.

use core::time::Duration;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Instant;

use str0m::net::{Protocol, Receive, Transmit};
use str0m::{Event, Input, Output, Rtc};

/// What an end of a call wants next.
#[derive(Debug)]
pub(crate) enum Stepped<T> {
    /// This is to be sent.
    Send(Transmit),
    /// This happened.
    Told(T),
    /// Nothing, until this time or until something arrives.
    Wait(Instant),
}

/// An end of a call, as whoever has its sockets and its clock has it:
/// it is given what arrived and what time it is, and says what it
/// wants next.
pub(crate) trait Core {
    /// What it says happened.
    type Told;

    /// Something arrived for the call, at `to`, from `from`. Returns
    /// whether it is the call's: a check that knows the call's
    /// password, or a datagram from where such a check came from.
    fn receive(
        &mut self,
        now: Instant,
        protocol: Protocol,
        from: SocketAddr,
        to: SocketAddr,
        datagram: &[u8],
    ) -> bool;

    /// What it wants next, at `now`.
    fn step(&mut self, now: Instant) -> Stepped<Self::Told>;
}

/// Longer than any call.
const FOREVER: Duration = Duration::from_hours(365 * 24);

/// How many times in one moment an end is told that its time has come
/// before whoever runs it is given the thread back. An end that is told
/// so says a later time to be asked at again, and is not told twice. If
/// `str0m` ever went on saying that its time is now, a loop that
/// believed it would hold the thread of every call beside this one.
pub(crate) const TURNS: u32 = 64;

/// When an end that had its [`TURNS`] is asked again.
pub(crate) const AGAIN: Duration = Duration::from_millis(1);

/// One end of a call, and what it wants sent.
pub(crate) struct Engine {
    rtc: Rtc,
    /// What is to be sent, the oldest first.
    out: VecDeque<Transmit>,
    /// When it wants to be given the time again.
    wake: Instant,
    over: bool,
    /// How many times it was given the time.
    #[cfg(test)]
    pub(crate) ticks: u32,
    /// How many more times it says that its time is now, whatever the
    /// library said: a library that does not move on.
    #[cfg(test)]
    pub(crate) stalls: u32,
}

impl Engine {
    pub(crate) fn new(rtc: Rtc, now: Instant) -> Self {
        Self {
            rtc,
            out: VecDeque::new(),
            wake: now,
            over: false,
            #[cfg(test)]
            ticks: 0,
            #[cfg(test)]
            stalls: 0,
        }
    }

    /// Whether the call is over: it was closed, by either end, or it
    /// failed.
    pub(crate) const fn is_over(&self) -> bool {
        self.over
    }

    /// Does `work` with the call. `None` for a call that is over, and
    /// for one that is over because of it.
    pub(crate) fn with<T>(&mut self, work: impl FnOnce(&mut Rtc) -> T) -> Option<T> {
        if self.over {
            return None;
        }
        let rtc = &mut self.rtc;
        let done = catch_unwind(AssertUnwindSafe(|| work(rtc))).ok();
        self.over |= done.is_none();
        done
    }

    /// Something arrived for the call, at `to`, from `from`. What it
    /// makes of it is to be asked for next ([`event`](Self::event)).
    ///
    /// Returns whether it is the call's. That is for the checks of the
    /// call to say: a check is the call's when it knows the password
    /// the far end was given, and anything else when it comes from
    /// where such a check came from. What is neither is not shown to
    /// the call at all. Its handshake would take a first word from
    /// anybody, and the far end's own would then find it taken.
    pub(crate) fn receive(
        &mut self,
        now: Instant,
        protocol: Protocol,
        from: SocketAddr,
        to: SocketAddr,
        datagram: &[u8],
    ) -> bool {
        let taken = self.with(|rtc| {
            // What is no datagram of a call is dropped, as the network
            // might have dropped it.
            let Ok(receive) = Receive::new(protocol, from, to, datagram) else {
                return false;
            };
            let input = Input::Receive(now, receive);
            if !rtc.accepts(&input) {
                return false;
            }
            let _ = rtc.handle_input(input);
            true
        });
        taken.unwrap_or(false)
    }

    /// Ends the call: the far end is told in the call's own ways, and
    /// what was said before still goes out. The call is over once that
    /// is done.
    ///
    /// `str0m` does not end a call whose data channel is still being
    /// set up: it says that it cannot, and has told the far end
    /// nothing by then. Such a call is over at once, and its far end
    /// finds it silent.
    pub(crate) fn close(&mut self) {
        let refused = self.with(|rtc| rtc.close().is_err());
        self.over |= refused == Some(true);
    }

    /// It is `now`. What comes of that is to be asked for next.
    pub(crate) fn tick(&mut self, now: Instant) {
        #[cfg(test)]
        {
            self.ticks += 1;
            self.stalls = self.stalls.saturating_sub(1);
        }
        self.with(|rtc| {
            let _ = rtc.handle_input(Input::Timeout(now));
        });
    }

    /// The next thing that happened. What is to be sent meanwhile is
    /// kept. `None` when nothing more happened: the call has said when
    /// it wants the time again ([`wake`](Self::wake)).
    pub(crate) fn event(&mut self) -> Option<Event> {
        loop {
            match self.with(Rtc::poll_output)? {
                Ok(Output::Transmit(transmit)) => self.out.push_back(transmit),
                Ok(Output::Event(event)) => return Some(event),
                Ok(Output::Timeout(at)) => {
                    self.wake = at;
                    // One that closed has said all it had to say.
                    self.over |= !self.rtc.is_alive();
                    return None;
                }
                Err(_) => {
                    self.over = true;
                    return None;
                }
            }
        }
    }

    /// The next datagram that is to be sent.
    pub(crate) fn transmit(&mut self) -> Option<Transmit> {
        self.out.pop_front()
    }

    /// When the call wants the time again.
    pub(crate) fn wake(&self, now: Instant) -> Instant {
        #[cfg(test)]
        if self.stalls > 0 && !self.over {
            return now;
        }
        if self.over { now + FOREVER } else { self.wake }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::Engine;
    use crate::crypto::certificate;
    use crate::stack::{self, Wanted};

    #[test]
    fn a_library_that_stops_ends_its_call_and_nothing_else() {
        let now = Instant::now();
        let certificate = certificate().unwrap();
        let mut engine = Engine::new(stack::rtc(&certificate, Wanted::default(), now), now);
        assert_eq!(engine.with(|_| 7), Some(7));
        assert!(!engine.is_over());
        // What the library does about something it did not expect.
        let stopped: Option<()> = engine.with(|_| panic!("the library stopped"));
        assert_eq!(stopped, None);
        assert!(engine.is_over());
        // It is asked nothing more, and has nothing more to say.
        let mut asked = false;
        assert_eq!(engine.with(|_| asked = true), None);
        assert!(!asked);
        engine.tick(now);
        assert!(engine.event().is_none());
        assert!(engine.transmit().is_none());
    }
}
