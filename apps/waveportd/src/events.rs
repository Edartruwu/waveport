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

use core::future::poll_fn;
use core::time::Duration;
use std::collections::VecDeque;
use std::sync::Arc;

use axum::response::sse::Event;
use serde::Serialize as _;
use tokio::sync::watch;
use tokio::time::{Instant, Interval, MissedTickBehavior};
use waveport_assembly::Told;
use waveport_domain::command::{Activity, ClientEvent};
use waveport_domain::ids::{GenId, Seq};
use waveport_ports::control::SessionEvent;
use waveport_ports::journal::SessionId;
use waveport_protocol::client::v1::{self as client, ByeReason, ErrorCode, FromServer, News, Show};
use waveport_protocol::journal;

use crate::daemon::{Shared, raised};

/// How long a client waits before it comes back to a stream that was
/// cut, as the stream tells it.
const RETRY: Duration = Duration::from_secs(2);

/// How often a session that told nothing more, and still runs, is
/// followed again before the stream is given up.
const FOLLOWED_AGAIN: u32 = 3;

/// Where a client that comes back says it was: the life of the session
/// it was told of, and the number of the last fact it was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Position {
    pub(crate) life: u64,
    pub(crate) seen: u64,
}

impl Position {
    /// Reads a position as the `id` of an event says it.
    pub(crate) fn read(written: &str) -> Option<Self> {
        let (life, seen) = written.trim().split_once('.')?;
        let plain = |digits: &str| digits.bytes().all(|byte| byte.is_ascii_digit());
        if !plain(life) || !plain(seen) {
            return None;
        }
        Some(Self {
            life: life.parse().ok()?,
            seen: seen.parse().ok()?,
        })
    }
}

/// What of a session's news a follower asked to be told.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Asked {
    /// The text of responses as it goes out.
    pub(crate) said: bool,
    /// What is recognised of what is being said.
    pub(crate) hearing: bool,
    /// What of the journal is shown beyond what was said.
    pub(crate) show: Show,
}

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
    session: SessionId,
    life: u64,
    asked: Asked,
    /// What the session tells, while it does.
    told: Option<Told>,
    /// The number of the last fact the session told this follower of,
    /// shown or not.
    seen: u64,
    /// What is ready to be sent.
    ready: VecDeque<Event>,
    going: watch::Receiver<bool>,
    keep_alive: Interval,
    /// How often in a row the session was followed again and told
    /// nothing.
    again: u32,
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
        let mut following = Self {
            going: daemon.going(),
            daemon,
            session,
            life,
            asked,
            told: None,
            seen: 0,
            ready: VecDeque::new(),
            keep_alive,
            again: 0,
            over: false,
        };
        following.ready.push_back(event(welcome, None).retry(RETRY));
        match (told, position) {
            (Some(told), None) => following.told = Some(told),
            (_, None) => following.follow_after(0),
            // What was told of another life this one may never have
            // done, and a fact this life has not made was told by
            // nobody: neither can be gone on from.
            (_, Some(position)) => {
                let made = daemon_made(&following.daemon, session);
                if position.life == life && made.is_some_and(|made| position.seen <= made) {
                    following.follow_after(position.seen);
                } else {
                    following.follow_from_state();
                }
            }
        }
        following
    }

    /// Follows the session from the fact after the one numbered `seen`.
    fn follow_after(&mut self, seen: u64) {
        self.seen = seen;
        self.told = self
            .daemon
            .stack
            .follow(self.session, Seq(seen.saturating_add(1)));
    }

    /// Follows the session from what it is now.
    fn follow_from_state(&mut self) {
        self.told = self.daemon.stack.follow_from_state(self.session);
    }

    fn say(&mut self, message: &FromServer, id: Option<u64>) {
        let id = id.map(|seq| (self.life, seq));
        self.ready.push_back(event(message, id));
    }

    /// The last event, and nothing after it.
    fn bye(&mut self, reason: ByeReason) {
        self.say(&FromServer::Bye { reason }, None);
        self.told = None;
        self.over = true;
    }

    /// Takes what the session told.
    fn take(&mut self, told: SessionEvent) {
        self.again = 0;
        let asked = self.asked;
        match told {
            SessionEvent::Fact { seq, record } => {
                self.seen = seq.get();
                let Ok(record) = serde_json::from_slice::<journal::Record<'_>>(&record) else {
                    return;
                };
                let shown = client::Fact::shown(&record.fact, &self.daemon.tools, asked.show);
                if let Some(fact) = shown {
                    let seq = seq.get();
                    self.say(&FromServer::Fact { seq, fact }, Some(seq));
                }
            }
            SessionEvent::Told(ClientEvent::UserSpeaking {
                who,
                turn,
                speaking,
            }) => {
                let news = News::UserSpeaking {
                    who: who.get(),
                    turn: turn.get(),
                    speaking,
                };
                self.say(&FromServer::Told(news), None);
            }
            SessionEvent::Told(ClientEvent::Agent {
                agent,
                generation,
                activity,
            }) => {
                let news = News::Agent {
                    agent: agent.get(),
                    generation: generation.map(GenId::get),
                    activity: match activity {
                        Activity::Listening => client::Activity::Listening,
                        Activity::Thinking => client::Activity::Thinking,
                        Activity::Speaking => client::Activity::Speaking,
                    },
                };
                self.say(&FromServer::Told(news), None);
            }
            // The stage of a session's life is a fact, and is told as
            // one.
            SessionEvent::Told(ClientEvent::Lifecycle(_)) => {}
            SessionEvent::Said { generation, text } => {
                if asked.said {
                    let generation = generation.get();
                    self.say(&FromServer::Told(News::Said { generation, text }), None);
                }
            }
            SessionEvent::Hearing { who, text } => {
                if asked.hearing {
                    let who = who.get();
                    self.say(&FromServer::Told(News::Hearing { who, text }), None);
                }
            }
            // The facts asked for are no longer all held. What the
            // session is now takes their place.
            SessionEvent::Gap { .. } => self.follow_from_state(),
            SessionEvent::State { upto, state } => {
                let upto = upto.get();
                self.seen = upto;
                let Ok(state) = serde_json::from_slice::<journal::State<'_>>(&state) else {
                    let message = "what the session has come to cannot be told".to_owned();
                    let code = ErrorCode::Internal;
                    self.say(&FromServer::Error { code, message }, None);
                    return;
                };
                let state = client::State::shown(&state, &self.daemon.tools, asked.show);
                let resumes = upto.saturating_add(1);
                self.say(&FromServer::Gap { resumes }, None);
                self.say(&FromServer::State(state), Some(upto));
            }
        }
    }

    /// The session tells this follower nothing more: it is over, or the
    /// follower was too slow for it and is to follow again.
    async fn told_no_more(&mut self) {
        self.told = None;
        if self.daemon.stack.status(self.session).is_none() || self.again >= FOLLOWED_AGAIN {
            self.bye(ByeReason::Ended);
            return;
        }
        // A session that has just ended is still there for a moment.
        self.again += 1;
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.follow_after(self.seen);
    }

    /// The next event, or `None` when the stream is over.
    pub(crate) async fn next(&mut self) -> Option<Event> {
        loop {
            if let Some(event) = self.ready.pop_front() {
                return Some(event);
            }
            if self.over {
                return None;
            }
            let Some(told) = &mut self.told else {
                self.told_no_more().await;
                continue;
            };
            tokio::select! {
                biased;
                () = raised(&mut self.going) => self.bye(ByeReason::GoingAway),
                next = poll_fn(|cx| told.as_mut().poll_next(cx)) => match next {
                    Some(told) => self.take(told),
                    None => self.told_no_more().await,
                },
                _ = self.keep_alive.tick() => return Some(Event::default().comment("")),
            }
        }
    }
}

/// The number of the last fact the session `session` has made, if it
/// runs.
fn daemon_made(daemon: &Shared, session: SessionId) -> Option<u64> {
    let status = daemon.stack.status(session)?;
    Some(status.next_seq.get().saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::Position;

    #[test]
    fn a_position_is_a_life_and_a_number() {
        let at = |life, seen| Some(Position { life, seen });
        assert_eq!(Position::read("1.0"), at(1, 0));
        assert_eq!(Position::read(" 12.3456 "), at(12, 3456));
        for none in ["", "1", "1.", ".1", "1.2.3", "a.1", "1.-2", "+1.2", "1.2e3"] {
            assert_eq!(Position::read(none), None, "{none:?}");
        }
        // A number that is too large to be one.
        assert_eq!(Position::read("1.99999999999999999999999"), None);
    }
}
