//! What a session tells, as the messages a client is sent.
//!
//! Whoever follows a session for a client is told what the session
//! does ([`SessionEvent`]), and says it to the client in the client
//! protocol ([`FromServer`]). What that comes to is the same on every
//! connection that carries the protocol, so it is here, once: which
//! facts a client sees, which news it asked for, where a client that
//! comes back is gone on from, and what takes the place of the facts a
//! session holds no longer.

use core::future::poll_fn;
use core::time::Duration;
use std::collections::VecDeque;

use waveport_domain::command::{Activity, ClientEvent};
use waveport_domain::ids::{GenId, Seq};
use waveport_ports::control::SessionEvent;
use waveport_ports::journal::SessionId;
use waveport_protocol::client::v1::{self as client, ErrorCode, FromServer, News, Show, features};
use waveport_protocol::journal;

use crate::stack::{Stack, Told};

/// How often a session that told nothing more, and still runs, is
/// followed again before it is given up.
const FOLLOWED_AGAIN: u32 = 3;

/// How long a session that told nothing more is left before it is
/// followed again: one that has just ended is still there for a moment.
const FOLLOWED_AGAIN_AFTER: Duration = Duration::from_millis(20);

/// What of a session's news a follower asked to be told.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Asked {
    /// The text of responses as it goes out.
    pub said: bool,
    /// What is recognised of what is being said.
    pub hearing: bool,
    /// What of the journal is shown beyond what was said.
    pub show: Show,
}

impl Asked {
    /// What a client asked for by the names of `wanted`, which a hello
    /// or an address gives. A name that is not a feature is passed over.
    /// What a tool was given and what it answered is shown to a client
    /// that asked to be told of tools only with `tool_text`: that is for
    /// whoever serves it to decide.
    pub fn of<'a>(wanted: impl IntoIterator<Item = &'a str>, tool_text: bool) -> Self {
        let mut asked = Self::default();
        for feature in wanted {
            match feature {
                features::SAID => asked.said = true,
                features::HEARING => asked.hearing = true,
                features::TOOLS => asked.show.tools = true,
                _ => {}
            }
        }
        asked.show.tool_text = asked.show.tools && tool_text;
        asked
    }

    /// The names of the features that will be told, as a welcome says
    /// them.
    #[must_use]
    pub fn features(&self) -> Vec<String> {
        let granted = [
            (features::SAID, self.said),
            (features::HEARING, self.hearing),
            (features::TOOLS, self.show.tools),
        ];
        granted
            .iter()
            .filter(|(_, told)| *told)
            .map(|(feature, _)| (*feature).to_owned())
            .collect()
    }
}

/// What one follower of a session is told, as messages for its client.
#[derive(Clone, Debug)]
pub struct Telling {
    asked: Asked,
    /// What the tools are called, in the kernel's order.
    tools: Vec<String>,
    /// The number of the last fact the session told this follower of,
    /// shown or not.
    seen: u64,
}

impl Telling {
    /// For a follower that asked for `asked` of a session whose tools
    /// are called `tools`, and was told of its facts up to `seen`.
    #[must_use]
    pub const fn new(asked: Asked, tools: Vec<String>, seen: u64) -> Self {
        Self { asked, tools, seen }
    }

    /// The number of the last fact the follower was told of, shown or
    /// not: where it is followed on from.
    #[must_use]
    pub const fn seen(&self) -> u64 {
        self.seen
    }

    /// The follower is told of the facts after `seen` from here on: it
    /// is where the session is followed from.
    pub const fn follows_after(&mut self, seen: u64) {
        self.seen = seen;
    }

    /// Takes what the session told. Each message there is for the
    /// client is given to `say`, with the number it is known by if it is
    /// a fact or a state: that is what a client names when it comes
    /// back.
    ///
    /// Returns whether the facts that were asked for are no longer all
    /// held: the session is then to be followed from what it has come
    /// to, which says what takes their place.
    pub fn take(
        &mut self,
        told: SessionEvent,
        mut say: impl FnMut(FromServer, Option<u64>),
    ) -> bool {
        let asked = self.asked;
        match told {
            SessionEvent::Fact { seq, record } => {
                self.seen = seq.get();
                let Ok(record) = serde_json::from_slice::<journal::Record<'_>>(&record) else {
                    return false;
                };
                if let Some(fact) = client::Fact::shown(&record.fact, &self.tools, asked.show) {
                    let seq = seq.get();
                    say(FromServer::Fact { seq, fact }, Some(seq));
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
                say(FromServer::Told(news), None);
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
                say(FromServer::Told(news), None);
            }
            // The stage of a session's life is a fact, and is told as
            // one.
            SessionEvent::Told(ClientEvent::Lifecycle(_)) => {}
            SessionEvent::Said { generation, text } => {
                if asked.said {
                    let generation = generation.get();
                    say(FromServer::Told(News::Said { generation, text }), None);
                }
            }
            SessionEvent::Hearing { who, text } => {
                if asked.hearing {
                    let who = who.get();
                    say(FromServer::Told(News::Hearing { who, text }), None);
                }
            }
            SessionEvent::Gap { .. } => return true,
            SessionEvent::State { upto, state } => {
                let upto = upto.get();
                self.seen = upto;
                let Ok(state) = serde_json::from_slice::<journal::State<'_>>(&state) else {
                    let message = "what the session has come to cannot be told".to_owned();
                    let code = ErrorCode::Internal;
                    say(FromServer::Error { code, message }, None);
                    return false;
                };
                let state = client::State::shown(&state, &self.tools, asked.show);
                let resumes = upto.saturating_add(1);
                say(FromServer::Gap { resumes }, None);
                say(FromServer::State(state), Some(upto));
            }
        }
        false
    }
}

/// Where a client that comes back says it was: the life of the session
/// it was told of, and the number of the last fact it was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    /// The life of the session.
    pub life: u64,
    /// The number of the last fact.
    pub seen: u64,
}

impl Position {
    /// Reads a position written as a life and a number with a point
    /// between them, as `2.17`.
    #[must_use]
    pub fn read(written: &str) -> Option<Self> {
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

/// One follower of a session, for a client: what the session tells it,
/// as the messages the client is sent, for as long as the session runs.
pub struct Follower {
    session: SessionId,
    telling: Telling,
    /// What the session tells, while it does.
    told: Option<Told>,
    /// What is ready to be sent, each with the number it is known by if
    /// it is a fact or a state.
    ready: VecDeque<(FromServer, Option<u64>)>,
    /// How often in a row the session was followed again and told
    /// nothing.
    again: u32,
}

impl core::fmt::Debug for Follower {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Follower")
            .field("session", &self.session)
            .field("seen", &self.telling.seen())
            .finish_non_exhaustive()
    }
}

impl Follower {
    /// A follower of the session `session` of `stack`, which is in the
    /// life `life`, for a client that asked for `asked`.
    ///
    /// `told` is what the session tells from its beginning, if it was
    /// started for this follower. Otherwise the session is followed
    /// from where `position` says the client was, or from its first
    /// fact. A client that was told of another life of the session, or
    /// names a fact this life has not made, cannot be gone on from: it
    /// is told what the session has come to, and what follows.
    #[must_use]
    pub fn begin(
        stack: &Stack,
        session: SessionId,
        life: u64,
        asked: Asked,
        told: Option<Told>,
        position: Option<Position>,
    ) -> Self {
        let mut follower = Self {
            session,
            telling: Telling::new(asked, stack.tools(), 0),
            told: None,
            ready: VecDeque::new(),
            again: 0,
        };
        match (told, position) {
            (Some(told), None) => follower.told = Some(told),
            (_, None) => follower.follow_after(stack, 0),
            (_, Some(position)) => {
                let made = stack
                    .status(session)
                    .map(|status| status.next_seq.get().saturating_sub(1));
                if position.life == life && made.is_some_and(|made| position.seen <= made) {
                    follower.follow_after(stack, position.seen);
                } else {
                    follower.told = stack.follow_from_state(session);
                }
            }
        }
        follower
    }

    /// The number of the last fact the follower was told of, shown or
    /// not.
    #[must_use]
    pub const fn seen(&self) -> u64 {
        self.telling.seen()
    }

    /// Follows the session from the fact after the one numbered `seen`.
    fn follow_after(&mut self, stack: &Stack, seen: u64) {
        self.telling.follows_after(seen);
        self.told = stack.follow(self.session, Seq(seen.saturating_add(1)));
    }

    /// The next message for the client, with the number it is known by
    /// if it is a fact or a state. `None` when the session tells
    /// nothing more: it is over.
    ///
    /// Whoever stops waiting for it loses nothing: it can be asked
    /// again.
    pub async fn next(&mut self, stack: &Stack) -> Option<(FromServer, Option<u64>)> {
        loop {
            if let Some(ready) = self.ready.pop_front() {
                return Some(ready);
            }
            let Some(told) = &mut self.told else {
                // It is over, or this follower was too slow for it and
                // is to follow again.
                if stack.status(self.session).is_none() || self.again >= FOLLOWED_AGAIN {
                    return None;
                }
                self.again += 1;
                tokio::time::sleep(FOLLOWED_AGAIN_AFTER).await;
                self.follow_after(stack, self.telling.seen());
                continue;
            };
            let Some(told) = poll_fn(|cx| told.as_mut().poll_next(cx)).await else {
                self.told = None;
                continue;
            };
            self.again = 0;
            let ready = &mut self.ready;
            let gap = self
                .telling
                .take(told, |message, id| ready.push_back((message, id)));
            // The facts asked for are no longer all held. What the
            // session is now takes their place.
            if gap {
                self.told = stack.follow_from_state(self.session);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use waveport_protocol::client::v1::Show;

    use super::{Asked, Position};

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

    #[test]
    fn a_client_is_told_what_it_asked_for_and_no_more() {
        let nothing = Asked::of([], true);
        assert_eq!(nothing, Asked::default());
        assert_eq!(nothing.features(), [] as [&str; 0]);

        let asked = Asked::of(["hearing", "dancing", "said"], true);
        assert!(asked.said && asked.hearing);
        // What a tool said is for a client that asked to be told of
        // tools, and for no other.
        assert_eq!(asked.show, Show::default());
        // In the order the protocol has them, whatever the order they
        // were asked in.
        assert_eq!(asked.features(), ["said", "hearing"]);

        let with_tools = Asked::of(["tools"], false);
        assert!(with_tools.show.tools && !with_tools.show.tool_text);
        assert_eq!(with_tools.features(), ["tools"]);
        let with_text = Asked::of(["tools"], true);
        assert!(with_text.show.tools && with_text.show.tool_text);
    }
}
