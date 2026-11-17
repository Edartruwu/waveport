//! What a session tells, as the server half of the client protocol over
//! server-sent events.
//!
//! Each event is one message a server says in the client protocol
//! ([`FromServer`]): the event is named as the message's `type`, and its
//! data is the message. A fact and a state carry an `id`, which is the
//! life of the session and the number of the fact, as `2.17`: a client
//! that comes back says the last one it was sent, which a browser does
//! by itself (`Last-Event-ID`).
//!
//! The first event is a `welcome`, and the last is a `bye`: a stream
//! that ends without one was cut, and is one to come back to.

use core::time::Duration;
use std::sync::Arc;

use axum::response::sse::Event;
use serde::Serialize as _;
use tokio::sync::watch;
use tokio::time::{Instant, Interval, MissedTickBehavior};
use waveport_assembly::Told;
use waveport_assembly::shown::{Asked, Follower, Position};
use waveport_ports::journal::SessionId;
use waveport_protocol::client::v1::{ByeReason, FromServer};

use crate::daemon::{Shared, raised};

/// How long a client waits before it comes back to a stream that was
/// cut, as the stream tells it.
const RETRY: Duration = Duration::from_secs(2);

/// One message as an event.
fn event(message: &FromServer, id: Option<(u64, u64)>) -> Event {
    let mut data = Vec::with_capacity(128);
    let written = message
        .serialize(&mut serde_json::Serializer::new(&mut data))
        .is_ok();
    let data = String::from_utf8(data).unwrap_or_default();
    // The name of the event is the message's own.
    let kind = match message {
        FromServer::Welcome { .. } => "welcome",
        FromServer::Mark { .. } => "mark",
        FromServer::Clear { .. } => "clear",
        FromServer::Fact { .. } => "fact",
        FromServer::Told(_) => "told",
        FromServer::Gap { .. } => "gap",
        FromServer::State(_) => "state",
        FromServer::Error { .. } => "error",
        FromServer::Bye { .. } => "bye",
        FromServer::Unknown => "unknown",
    };
    let mut event = Event::default().event(kind);
    if let Some((life, seq)) = id {
        event = event.id(format!("{life}.{seq}"));
    }
    if written {
        event.data(data)
    } else {
        event.data("{}")
    }
}

/// The stream of one follower.
pub(crate) struct Following {
    daemon: Arc<Shared>,
    life: u64,
    /// What the session tells, as what the client is sent.
    follower: Follower,
    /// The welcome, until it is sent.
    welcome: Option<Event>,
    going: watch::Receiver<bool>,
    keep_alive: Interval,
    /// Whether the last event was sent.
    over: bool,
}

impl Following {
    /// A stream for a follower of the session `session`, which is in the
    /// life `life`. It begins with `welcome`. `told` is what the session
    /// tells from its beginning, if it was started for this follower.
    /// Otherwise the session is followed from where `position` says the
    /// follower was, or from its first fact.
    pub(crate) fn begin(
        daemon: Arc<Shared>,
        session: SessionId,
        life: u64,
        asked: Asked,
        welcome: &FromServer,
        told: Option<Told>,
        position: Option<Position>,
    ) -> Self {
        // Something is sent on a stream that has nothing to say, so
        // that whatever stands between the daemon and the client does
        // not take the stream for dead.
        let every = Duration::from_millis(u64::from(daemon.limits.keep_alive_ms.max(1)));
        let mut keep_alive = tokio::time::interval_at(Instant::now() + every, every);
        keep_alive.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let follower = Follower::begin(&daemon.stack, session, life, asked, told, position);
        Self {
            going: daemon.going(),
            daemon,
            life,
            follower,
            welcome: Some(event(welcome, None).retry(RETRY)),
            keep_alive,
            over: false,
        }
    }

    /// The last event, and nothing after it.
    fn bye(&mut self, reason: ByeReason) -> Event {
        self.over = true;
        event(&FromServer::Bye { reason }, None)
    }

    /// The next event, or `None` when the stream is over.
    pub(crate) async fn next(&mut self) -> Option<Event> {
        if let Some(welcome) = self.welcome.take() {
            return Some(welcome);
        }
        if self.over {
            return None;
        }
        let life = self.life;
        tokio::select! {
            biased;
            () = raised(&mut self.going) => Some(self.bye(ByeReason::GoingAway)),
            next = self.follower.next(&self.daemon.stack) => match next {
                Some((message, id)) => Some(event(&message, id.map(|seq| (life, seq)))),
                None => Some(self.bye(ByeReason::Ended)),
            },
            _ = self.keep_alive.tick() => Some(Event::default().comment("")),
        }
    }
}
