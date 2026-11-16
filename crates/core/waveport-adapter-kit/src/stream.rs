//! The two halves of a stream port, from one ordinary loop.
//!
//! A streaming adapter is a loop: wait for a command or for a message
//! from the provider, whichever comes first, and act on it. [`pair`]
//! takes such a loop, written as an `async` block, and gives the two
//! halves the ports ask for.
//!
//! - The [`Sender`] takes commands without waiting and hands them to the
//!   loop. When the loop is behind by more than the room it was given,
//!   the sender says [`Full`].
//! - The [`Receiver`] owns the loop. Polling the receiver is what runs
//!   it, so the adapter starts no task, and dropping the receiver drops
//!   the loop with everything it holds: the connection closes and the
//!   stream is cancelled.
//!
//! The loop gives its events to an [`Events`] handle and waits there
//! when the receiver has not taken the ones before, so a consumer that is
//! slow slows the loop down, and through it the provider's socket.

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::mpsc;
use waveport_ports::error::{Full, PortError};
use waveport_ports::llm::{LlmEvent, LlmRx};
use waveport_ports::stt::{SttEvent, SttRx};
use waveport_ports::tts::{TtsEvent, TtsRx};

/// How many events a loop may give in one go before it has to wait for
/// the receiver to take them.
const EVENTS_AT_ONCE: usize = 16;

type Queue<E> = Arc<Mutex<VecDeque<E>>>;
type Loop = Pin<Box<dyn Future<Output = Result<(), PortError>> + Send>>;

/// Builds the two halves of a stream from its loop.
///
/// `run` is given the commands that arrive and a handle to give events
/// to. The stream ends when the future it returns does: with `Ok`, the
/// receiver gives the events that are left and then `None`; with an
/// error, the receiver gives that error last.
///
/// `room` is how many commands may wait for the loop.
pub fn pair<C, E, F, Fut>(room: usize, run: F) -> (Sender<C>, Receiver<E>)
where
    F: FnOnce(Commands<C>, Events<E>) -> Fut,
    Fut: Future<Output = Result<(), PortError>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel(room.max(1));
    let queue: Queue<E> = Arc::new(Mutex::new(VecDeque::with_capacity(EVENTS_AT_ONCE)));
    let events = Events {
        queue: Arc::clone(&queue),
    };
    let work: Loop = Box::pin(run(Commands { rx }, events));
    (
        Sender { tx },
        Receiver {
            work: Some(work),
            queue,
        },
    )
}

/// A stream with nothing to send on: a loop and its events.
pub fn events<E, F, Fut>(run: F) -> Receiver<E>
where
    F: FnOnce(Events<E>) -> Fut,
    Fut: Future<Output = Result<(), PortError>> + Send + 'static,
{
    let (_, receiver) = pair::<(), E, _, _>(1, |_, events| run(events));
    receiver
}

/// The half of a stream that takes commands.
pub struct Sender<C> {
    tx: mpsc::Sender<C>,
}

impl<C> Sender<C> {
    /// Hands a command to the loop.
    ///
    /// # Errors
    ///
    /// [`Full`]: as many commands as there is room for are waiting, or the
    /// loop has ended and takes none.
    pub fn try_send(&mut self, command: C) -> Result<(), Full> {
        self.tx.try_send(command).map_err(|_| Full)
    }

    /// Whether the loop has ended, so that nothing sent would be taken.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

impl<C> fmt::Debug for Sender<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender").finish_non_exhaustive()
    }
}

/// The commands a loop is given.
pub struct Commands<C> {
    rx: mpsc::Receiver<C>,
}

impl<C> Commands<C> {
    /// The next command. `None` means the sender is gone and no command
    /// will come; a loop that only waits for commands should end then.
    pub async fn next(&mut self) -> Option<C> {
        self.rx.recv().await
    }

    /// The next command if one is waiting.
    pub fn try_next(&mut self) -> Option<C> {
        self.rx.try_recv().ok()
    }
}

impl<C> fmt::Debug for Commands<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Commands").finish_non_exhaustive()
    }
}

/// Where a loop gives its events.
pub struct Events<E> {
    queue: Queue<E>,
}

impl<E> Events<E> {
    /// Gives an event to the receiver, waiting first if the receiver has
    /// not taken the ones before.
    pub async fn send(&mut self, event: E) {
        let mut event = Some(event);
        core::future::poll_fn(|_| {
            let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
            if queue.len() >= EVENTS_AT_ONCE {
                // No waker is needed: the receiver polls the loop again
                // as soon as it has taken what is waiting.
                return Poll::Pending;
            }
            if let Some(event) = event.take() {
                queue.push_back(event);
            }
            Poll::Ready(())
        })
        .await;
    }
}

impl<E> fmt::Debug for Events<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Events").finish_non_exhaustive()
    }
}

/// The half of a stream that gives events, and that runs the loop.
pub struct Receiver<E> {
    /// The loop, until it has ended.
    work: Option<Loop>,
    queue: Queue<E>,
}

impl<E> Receiver<E> {
    /// The next event. `None` means the stream is over. An error is the
    /// last thing a stream gives.
    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<E, PortError>>> {
        if let Some(event) = self.take() {
            return Poll::Ready(Some(Ok(event)));
        }
        let Some(work) = self.work.as_mut() else {
            return Poll::Ready(None);
        };
        let ended = work.as_mut().poll(cx);
        if ended.is_ready() {
            // Dropped here, so that the connection closes now and not
            // when the receiver is dropped.
            self.work = None;
        }
        // What the loop gave before it ended, or before it waited, comes
        // first.
        if let Some(event) = self.take() {
            if let Poll::Ready(Err(error)) = ended {
                self.failed(error);
            }
            return Poll::Ready(Some(Ok(event)));
        }
        match ended {
            Poll::Ready(Ok(())) => Poll::Ready(None),
            Poll::Ready(Err(error)) => Poll::Ready(Some(Err(error))),
            Poll::Pending => Poll::Pending,
        }
    }

    /// The next event, as a future.
    pub async fn next(&mut self) -> Option<Result<E, PortError>> {
        core::future::poll_fn(|cx| self.poll_next(cx)).await
    }

    fn take(&mut self) -> Option<E> {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }

    /// Keeps an error to give once the events before it are taken.
    fn failed(&mut self, error: PortError) {
        self.work = Some(Box::pin(async move { Err(error) }));
    }
}

impl<E> fmt::Debug for Receiver<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver")
            .field("ended", &self.work.is_none())
            .finish_non_exhaustive()
    }
}

impl SttRx for Receiver<SttEvent> {
    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<SttEvent, PortError>>> {
        Receiver::poll_next(self.get_mut(), cx)
    }
}

impl LlmRx for Receiver<LlmEvent> {
    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<LlmEvent, PortError>>> {
        Receiver::poll_next(self.get_mut(), cx)
    }
}

impl TtsRx for Receiver<TtsEvent> {
    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<TtsEvent, PortError>>> {
        Receiver::poll_next(self.get_mut(), cx)
    }
}

/// Which of two things happened first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Either<A, B> {
    /// The first.
    Left(A),
    /// The second.
    Right(B),
}

/// Waits for whichever of two futures is done first. When both are, the
/// first one wins: the order is fixed and nothing is left to chance, so a
/// loop behaves the same every time it is given the same input.
///
/// The future that did not win is dropped. Use it with futures that lose
/// nothing by that, as [`Commands::next`] and reading the next message of
/// a connection do.
pub async fn first<A, B>(a: A, b: B) -> Either<A::Output, B::Output>
where
    A: Future,
    B: Future,
{
    let mut a = core::pin::pin!(a);
    let mut b = core::pin::pin!(b);
    core::future::poll_fn(|cx| {
        if let Poll::Ready(output) = a.as_mut().poll(cx) {
            return Poll::Ready(Either::Left(output));
        }
        if let Poll::Ready(output) = b.as_mut().poll(cx) {
            return Poll::Ready(Either::Right(output));
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use core::future::Future;
    use core::pin::Pin;
    use core::task::{Context, Poll, Waker};
    use core::time::Duration;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use waveport_ports::error::{Full, PortError};

    use super::{EVENTS_AT_ONCE, Either, events, first, pair};

    /// Polls a future once, with a waker that does nothing.
    fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }

    #[tokio::test]
    async fn commands_go_in_and_events_come_out() {
        let (mut sender, mut receiver) = pair(4, |mut commands, mut events| async move {
            while let Some(n) = commands.next().await {
                events.send(n * 2).await;
                events.send(n * 2 + 1).await;
            }
            Ok(())
        });
        sender.try_send(1_u32).unwrap();
        sender.try_send(5).unwrap();
        let mut heard = Vec::new();
        for _ in 0..4 {
            heard.push(receiver.next().await.unwrap().unwrap());
        }
        assert_eq!(heard, [2, 3, 10, 11]);
        // With the sender gone the loop ends, and so does the stream.
        drop(sender);
        assert_eq!(receiver.next().await, None);
        assert_eq!(receiver.next().await, None, "and stays over");
    }

    #[tokio::test]
    async fn a_sender_with_no_room_says_so() {
        let (mut sender, mut receiver) = pair(2, |mut commands, mut events| async move {
            while let Some(n) = commands.next().await {
                events.send(n).await;
            }
            Ok(())
        });
        // Nothing runs the loop yet, so the room fills.
        assert_eq!(sender.try_send(1_u8), Ok(()));
        assert_eq!(sender.try_send(2), Ok(()));
        assert_eq!(sender.try_send(3), Err(Full));
        // Taking an event runs the loop, which makes room.
        assert_eq!(receiver.next().await, Some(Ok(1)));
        assert_eq!(receiver.next().await, Some(Ok(2)));
        assert_eq!(sender.try_send(3), Ok(()));
        assert_eq!(receiver.next().await, Some(Ok(3)));
        assert!(!sender.is_closed());
    }

    #[tokio::test]
    async fn an_error_is_the_last_thing_a_stream_gives() {
        let mut receiver = events(|mut events| async move {
            events.send("one").await;
            events.send("two").await;
            Err(PortError::transient("the connection dropped"))
        });
        assert_eq!(receiver.next().await, Some(Ok("one")));
        assert_eq!(receiver.next().await, Some(Ok("two")));
        assert_eq!(
            receiver.next().await,
            Some(Err(PortError::transient("the connection dropped")))
        );
        assert_eq!(receiver.next().await, None);
    }

    #[tokio::test]
    async fn a_loop_that_gives_much_waits_for_the_receiver() {
        let given = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&given);
        let mut receiver = events(|mut events| async move {
            for n in 0..100_usize {
                events.send(n).await;
                counter.store(n + 1, Ordering::SeqCst);
            }
            Ok(())
        });
        assert_eq!(receiver.next().await, Some(Ok(0)));
        // The loop ran until the receiver held as much as it may.
        assert_eq!(given.load(Ordering::SeqCst), EVENTS_AT_ONCE);
        let mut heard = vec![0];
        while let Some(event) = receiver.next().await {
            heard.push(event.unwrap());
        }
        assert_eq!(heard, (0..100).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn dropping_the_receiver_drops_the_loop() {
        struct Connection(Arc<AtomicBool>);
        impl Drop for Connection {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let closed = Arc::new(AtomicBool::new(false));
        let connection = Connection(Arc::clone(&closed));
        let (mut sender, mut receiver) = pair(2, |mut commands, mut events| async move {
            let _connection = connection;
            while let Some(n) = commands.next().await {
                events.send(n).await;
            }
            Ok(())
        });
        sender.try_send(1_u8).unwrap();
        assert_eq!(receiver.next().await, Some(Ok(1)));
        assert!(!closed.load(Ordering::SeqCst));
        drop(receiver);
        assert!(closed.load(Ordering::SeqCst), "the connection is closed");
        // What is sent now goes nowhere, and the sender knows.
        assert!(sender.is_closed());
        assert_eq!(sender.try_send(2), Err(Full));
    }

    #[tokio::test]
    async fn a_loop_that_ends_closes_its_connection_at_once() {
        struct Connection(Arc<AtomicBool>);
        impl Drop for Connection {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let closed = Arc::new(AtomicBool::new(false));
        let connection = Connection(Arc::clone(&closed));
        let mut receiver = events(|mut events| async move {
            let _connection = connection;
            events.send(7_u8).await;
            Ok(())
        });
        assert_eq!(receiver.next().await, Some(Ok(7)));
        assert!(closed.load(Ordering::SeqCst));
        assert_eq!(receiver.next().await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_receiver_that_waits_is_woken_by_a_command() {
        let (mut sender, mut receiver) = pair(2, |mut commands, mut events| async move {
            while let Some(n) = commands.next().await {
                events.send(n).await;
            }
            Ok(())
        });
        let waiting = tokio::spawn(async move { receiver.next().await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!waiting.is_finished());
        sender.try_send(9_u8).unwrap();
        assert_eq!(waiting.await.unwrap(), Some(Ok(9)));
    }

    #[tokio::test]
    async fn the_first_of_two_wins_and_a_tie_goes_to_the_first() {
        let ready = core::future::ready(1);
        let never = core::future::pending::<u8>();
        assert_eq!(first(ready, never).await, Either::Left(1));
        let never = core::future::pending::<u8>();
        assert_eq!(first(never, core::future::ready(2)).await, Either::Right(2));
        let tie = first(core::future::ready(1), core::future::ready(2)).await;
        assert_eq!(tie, Either::Left(1));

        let both = first(core::future::pending::<u8>(), core::future::pending::<u8>());
        let mut both = core::pin::pin!(both);
        assert!(poll_once(both.as_mut()).is_pending());
    }
}
