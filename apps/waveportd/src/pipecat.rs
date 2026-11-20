//! What a client of Pipecat's asks over HTTP: the signalling of its
//! `SmallWebRTC` transport, so that an application written for that
//! client calls an agent here by changing an address (decision 0006).
//!
//! | Request | Answer |
//! |---|---|
//! | `POST /start`, `{transport: "webrtc"}` | 200, `{sessionId, iceConfig?}`: a name to make an offer under, and the servers a far end is told of, where there are any |
//! | `POST /api/offer` and `POST /sessions/{sessionId}/api/offer`, `{sdp, type, pc_id, restart_pc}` | 200, `{sdp, type, pc_id}`: the answer, and the name of the call |
//! | `PATCH` on the same, `{pc_id, candidates}` | 200, `{"status": "success"}`, or 404 for a call that is not known |
//!
//! These are Pipecat's routes and Pipecat's bodies, as read from the
//! source of Pipecat 1.16.0 and of `@pipecat-ai/small-webrtc-transport`
//! 1.11.2, and they stand beside the daemon's own (`POST /v1/webrtc`).
//! What is refused is answered as everywhere else, with a problem: the
//! client goes by the status. A status of the 400s ends its call for
//! good, and one of the 500s is tried again two seconds later.
//!
//! On the call the client speaks RTVI on a data channel it opens, and
//! [`Gateway`] puts that into client protocol v1 and back.
//!
//! # Who may ask
//!
//! - **Whoever holds a client token**, said as `Authorization: Bearer`.
//!   The offer is for the session the token was made for. The token is
//!   used up when the far end has come on its call, and not before: a
//!   call that never got through can be made again.
//! - **Whoever holds a control key**, or anyone where the daemon has no
//!   key, which is a daemon that listens on this machine alone. A
//!   session is made of the agent the address names (`?agent=`), or of
//!   the agent the daemon has if it has one. A control key does not
//!   belong in a page that others are served.
//! - **Whoever holds the name of a call** (`pc_id`) and says what the
//!   call's offer was made with. An offer that names a call it knows
//!   is that call's: its description is agreed anew, or, with
//!   `restart_pc`, another call takes its place. This is how the client
//!   comes back when its network failed. A call that is over is known
//!   for as long as its session waits for its far end.
//!
//! **The `sessionId` of `/start` is in the address of the offer that
//! follows, and an address is written down by whatever stands
//! between.** It is therefore not the session and not a token: it is a
//! name made up for this, good for as long as a client token is, and
//! only together with what `/start` was asked with.
//!
//! # What is not done
//!
//! Where the far end says it is (`candidates`) is not needed: the
//! daemon is heard from wherever a far end's checks come from. And
//! there is no route that ends a call: the session ends when the client
//! says `disconnect-bot`, or when its call is gone for longer than a
//! line that dropped is waited for.
//!
//! This client does not say what it has played, so how much of a
//! response was heard is worked out from the pace, as for any far end
//! that does not say.

use core::time::Duration;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use waveport_assembly::shown::{Asked, Position};
use waveport_ports::journal::SessionId;
use waveport_protocol::client::v1::{CLIENT_TOKEN, ErrorCode, features};
use waveport_protocol::control::v1::Problem;
use waveport_transport_webrtc::{Knock, Unanswered};
use waveport_transport_websocket::Hello;

use crate::auth::{digest, same};
use crate::daemon::{Shared, raised};
use crate::http::{Refused, bearer, control, json, not_admitted, read, session, values};
use crate::line::{self, Caller};
use crate::rtvi::Gateway;
use crate::webrtc::{self, IceServer, answering, no_call, unanswered};

/// What a name stands for, and for whom.
struct Named {
    session: SessionId,
    /// The hash of what whoever was given the name said of who they
    /// are.
    by: [u8; 32],
    until: Instant,
}

/// Names that stand for sessions for a while: one for a session at a
/// time.
#[derive(Default)]
struct Names(Mutex<HashMap<String, Named>>);

impl Names {
    /// `name` stands for `session` for the time `good_for`, for
    /// whoever says what hashes to `by`. A name the session had before
    /// stands for it no more.
    fn keep(&self, name: String, session: SessionId, by: [u8; 32], good_for: Duration) {
        let now = Instant::now();
        // A time too far off to be reckoned is as far off as can be.
        let until = now.checked_add(good_for).unwrap_or(now);
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        held.retain(|_, named| named.until > now && named.session != session);
        held.insert(name, Named { session, by, until });
    }

    /// The session `name` stands for, for whoever says what hashes to
    /// `by`.
    fn names(&self, name: &str, by: &[u8; 32]) -> Option<SessionId> {
        let held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        held.get(name)
            .filter(|named| named.until > Instant::now() && same(&named.by, by))
            .map(|named| named.session)
    }
}

/// What the clients of Pipecat's are known by, besides their calls.
#[derive(Default)]
pub(crate) struct Known {
    /// What `/start` named, for the offer that follows.
    started: Names,
    /// The calls that are over, for a client that calls again.
    left: Names,
}

/// What `/start` is asked with.
#[derive(Deserialize)]
struct Starts {
    #[serde(default)]
    transport: Option<String>,
}

/// What `/start` answers.
#[derive(Serialize)]
struct Started {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "iceConfig", skip_serializing_if = "Option::is_none")]
    ice_config: Option<IceConfig>,
}

#[derive(Serialize)]
struct IceConfig {
    #[serde(rename = "iceServers")]
    ice_servers: Vec<IceServer>,
}

/// An offer, as this client posts it.
#[derive(Deserialize)]
struct Offered {
    sdp: String,
    #[serde(rename = "type")]
    kind: String,
    /// The call the offer is for, if it is for one there was.
    #[serde(default)]
    pc_id: Option<String>,
    /// Whether another call is to take the place of that one.
    #[serde(default)]
    restart_pc: Option<bool>,
}

/// The answer to it.
#[derive(Serialize)]
struct Answer<'a> {
    sdp: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    pc_id: &'a str,
}

/// Where a far end says it is.
#[derive(Deserialize)]
struct Trickled {
    #[serde(default)]
    pc_id: Option<String>,
}

#[derive(Serialize)]
struct Heard {
    status: &'static str,
}

/// The hash of what whoever asks says of who they are: of nothing,
/// where they say nothing.
fn said_by(headers: &HeaderMap) -> Result<[u8; 32], Problem> {
    Ok(digest(bearer(headers)?.unwrap_or_default()))
}

/// The client token whoever asks says, if they say one.
fn client_token(headers: &HeaderMap) -> Result<Option<&str>, Problem> {
    Ok(bearer(headers)?.filter(|token| token.starts_with(CLIENT_TOKEN)))
}

/// A session a request is let into.
struct Admitted {
    session: SessionId,
    /// The token that is used up once the far end has come.
    ticket: String,
    /// Whether the session was made for this request.
    made: bool,
}

impl Admitted {
    /// What the request asked could not be done: a session that was
    /// made for it is let go of, and `problem` is why.
    async fn undone(self, daemon: &Shared, problem: Problem) -> Refused {
        if self.made {
            let _ = daemon.end(self.session).await;
        }
        problem.into()
    }
}

/// The session a request is let into that names no call and nothing
/// that was started: the one its client token was made for, or a new
/// one of the agent its address names, for whoever may make one.
fn asks(daemon: &Shared, headers: &HeaderMap, query: Option<&str>) -> Result<Admitted, Problem> {
    if let Some(token) = client_token(headers)? {
        let session = daemon.tickets.names(token).ok_or_else(|| {
            not_admitted("this is no client token: it was used, is too old, or was never one")
        })?;
        return Ok(Admitted {
            session,
            ticket: token.to_owned(),
            made: false,
        });
    }
    control(daemon, headers)?;
    let agent = match (
        values(query, "agent").next(),
        daemon.agent_names().as_slice(),
    ) {
        (Some(named), _) => named,
        (None, [only]) => (*only).to_owned(),
        (None, _) => {
            return Err(Problem::of(
                ErrorCode::NoAgent,
                "the daemon has more than one agent: the address says which, as `?agent=`",
            ));
        }
    };
    let created = daemon.create(&agent)?;
    Ok(Admitted {
        session: session(&created.session)?,
        ticket: created.client_token,
        made: true,
    })
}

/// Answers a client that asks for a session to be there before it
/// makes its offer.
pub(crate) async fn start(
    State(daemon): State<Arc<Shared>>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, Refused> {
    if daemon.is_draining() {
        return Err(Shared::busy().into());
    }
    let webrtc = answering(&daemon)?;
    let asked: Starts = read(&daemon, &headers, body).await?;
    if asked.transport.is_some_and(|named| named != "webrtc") {
        return Err(Problem::of(
            ErrorCode::Unsupported,
            "a client of Pipecat's is served on its `webrtc` transport, and on no other",
        )
        .into());
    }
    let by = said_by(&headers)?;
    let admitted = asks(&daemon, &headers, query.as_deref())?;
    let session = admitted.session;
    if let Err(problem) = daemon.takes_a_call(session) {
        return Err(admitted.undone(&daemon, problem).await);
    }
    let name = webrtc::name()?;
    let good_for = daemon.connect_within();
    webrtc
        .pipecat
        .started
        .keep(name.clone(), session, by, good_for);
    let servers = webrtc.ice_servers(session);
    // A client that is told of no servers keeps the ones it was set up
    // with, and one that is told of an empty list has none.
    let ice_config = (!servers.is_empty()).then_some(IceConfig {
        ice_servers: servers,
    });
    let started = Started {
        session_id: name,
        ice_config,
    };
    Ok(json(StatusCode::OK, &started))
}

/// Answers an offer that names nothing that was started.
pub(crate) async fn offer(
    State(daemon): State<Arc<Shared>>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, Refused> {
    offered(daemon, None, query.as_deref(), &headers, body).await
}

/// Answers an offer under the name `/start` gave.
pub(crate) async fn offer_of(
    State(daemon): State<Arc<Shared>>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, Refused> {
    offered(daemon, Some(&name), query.as_deref(), &headers, body).await
}

fn answer_of(answer: &str, call: &str) -> Response {
    let answer = Answer {
        sdp: answer,
        kind: "answer",
        pc_id: call,
    };
    json(StatusCode::OK, &answer)
}

async fn offered(
    daemon: Arc<Shared>,
    name: Option<&str>,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Body,
) -> Result<Response, Refused> {
    if daemon.is_draining() {
        return Err(Shared::busy().into());
    }
    let webrtc = answering(&daemon)?;
    let asked: Offered = read(&daemon, headers, body).await?;
    // A description says first which version it is of, and there is
    // one (RFC 8866, section 5.1).
    if asked.kind != "offer" || !asked.sdp.starts_with("v=0") {
        return Err(unanswered(Unanswered::Unreadable).into());
    }
    let by = said_by(headers)?;
    // An offer for a call that whoever asks made before is for the
    // session of that call.
    let mut again = None;
    if let Some(call) = &asked.pc_id {
        let held = webrtc.held(call, &by, |held| (held.signals.clone(), held.session));
        if let Some((signals, session)) = held {
            if asked.restart_pc == Some(true) {
                // Another call takes the place of this one, when its
                // far end has come: until then this one is the
                // session's, and is its far end's no more.
                webrtc.forget(call);
            } else {
                match offered_again(signals.offer(&asked.sdp).await) {
                    Ok(Some(answer)) => return Ok(answer_of(&answer, call)),
                    Ok(None) => {}
                    Err(other) => return Err(unanswered(other).into()),
                }
            }
            again = Some(session);
        } else {
            again = webrtc.pipecat.left.names(call, &by);
        }
    }
    let mut made = None;
    let (session, ticket) = match (again, name) {
        (Some(session), _) => (session, None),
        (None, Some(name)) => {
            let session = webrtc.pipecat.started.names(name, &by).ok_or_else(|| {
                Problem::of(
                    ErrorCode::NoSession,
                    "nothing was started under that name for whoever asks, or it was too long \
                     ago: a name is good for as long as a client token is",
                )
            })?;
            (session, client_token(headers)?.map(str::to_owned))
        }
        (None, None) => {
            let admitted = asks(&daemon, headers, query)?;
            let admitted = made.insert(admitted);
            (admitted.session, Some(admitted.ticket.clone()))
        }
    };
    let gateway = Box::new(Gateway::new());
    let answered = daemon
        .takes_a_call(session)
        .and_then(|()| webrtc.answer(session, by, &asked.sdp, Some(gateway)));
    let (call, answer, knock) = match (answered, made) {
        (Ok(answered), _) => answered,
        // A session that was made for an offer that is not answered is
        // nobody's.
        (Err(problem), Some(made)) => return Err(made.undone(&daemon, problem).await),
        (Err(problem), None) => return Err(problem.into()),
    };
    let response = answer_of(&answer, &call);
    let leaves = Leaves {
        daemon: Arc::clone(&daemon),
        call,
        session,
        by,
    };
    tokio::spawn(serve(leaves, knock, ticket));
    Ok(response)
}

/// What lets go of a call when whoever served it is done, and keeps in
/// mind whose it was for a client that calls again.
struct Leaves {
    daemon: Arc<Shared>,
    call: String,
    session: SessionId,
    by: [u8; 32],
}

impl Drop for Leaves {
    fn drop(&mut self) {
        let Some(webrtc) = &self.daemon.webrtc else {
            return;
        };
        webrtc.forget(&self.call);
        // A session that is over, or never began, waits for nobody.
        if let Some(waits) = self.daemon.waits_for(self.session) {
            let call = core::mem::take(&mut self.call);
            webrtc.pipecat.left.keep(call, self.session, self.by, waits);
        }
    }
}

/// One call of a client of Pipecat's, from the answer to its offer to
/// its end. `ticket` is the token the offer was made with, if it is one
/// that is used up.
async fn serve(leaves: Leaves, knock: Knock, ticket: Option<String>) {
    let daemon = Arc::clone(&leaves.daemon);
    let session = leaves.session;
    let Some(webrtc) = &daemon.webrtc else {
        return;
    };
    let mut going = daemon.going();
    let came = tokio::select! {
        came = knock.connect(webrtc.there_within()) => came,
        // A daemon that is going away waits for nobody.
        () = raised(&mut going) => return,
    };
    let Ok(caller) = came else {
        return;
    };
    webrtc.is_there(&leaves.call);
    // The token is used up by the far end that came with it, and by no
    // other after that.
    if ticket.is_some_and(|ticket| !daemon.tickets.redeem(&ticket, session)) {
        let why = "the token the offer was made with has been used, or is too old";
        caller.refuse(ErrorCode::Token, why).await;
        return;
    }
    // This client says no hello. It is told what RTVI has a word for:
    // text as it goes out, words as they are recognised, and that tools
    // are called, without what they were given and what they answered,
    // which no client is shown.
    let wanted = [features::SAID, features::HEARING, features::TOOLS];
    let asked = Asked::of(wanted, false);
    let hello = Hello {
        features: wanted.map(str::to_owned).to_vec(),
        ..Hello::new(None)
    };
    // Where the session is, if it is under way: a far end that comes
    // back cannot say where it was, and is told what happens from here
    // on and nothing twice.
    let seen = daemon
        .stack
        .status(session)
        .map(|status| status.next_seq.get().saturating_sub(1));
    let caller = Caller::Call(Box::new(caller), hello);
    let Some((link, life)) = daemon.on_line(session, caller, None, asked).await else {
        return;
    };
    let back = seen.map(|seen| Position { life, seen });
    line::tells(&daemon, session, life, asked, link, back).await;
}

/// Hears where a far end says it is.
pub(crate) async fn candidates(
    State(daemon): State<Arc<Shared>>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, Refused> {
    let webrtc = answering(&daemon)?;
    let asked: Trickled = read(&daemon, &headers, body).await?;
    let by = said_by(&headers)?;
    let call = asked.pc_id.unwrap_or_default();
    webrtc.held(&call, &by, |_| ()).ok_or_else(no_call)?;
    // It is heard from wherever its checks come from, so that is not
    // needed, and not kept.
    Ok(json(StatusCode::OK, &Heard { status: "success" }))
}

/// The same, asked under the name `/start` gave: the call says whose it
/// is.
pub(crate) async fn candidates_of(
    daemon: State<Arc<Shared>>,
    Path(_name): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, Refused> {
    candidates(daemon, headers, body).await
}

/// What became of an offer for a call that is there: its answer, or
/// none where the call ended meanwhile. That is no fault of the offer's
/// and is not told as one: another call is made for the session.
fn offered_again<T>(answered: Result<T, Unanswered>) -> Result<Option<T>, Unanswered> {
    match answered {
        Ok(answer) => Ok(Some(answer)),
        Err(Unanswered::Over) => Ok(None),
        Err(other) => Err(other),
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use waveport_ports::journal::SessionId;
    use waveport_transport_webrtc::Unanswered;

    use super::{Names, offered_again};

    #[test]
    fn a_call_that_ended_while_it_was_offered_again_is_no_fault_of_the_offer_s() {
        assert_eq!(offered_again(Ok("an answer")), Ok(Some("an answer")));
        assert_eq!(offered_again::<&str>(Err(Unanswered::Over)), Ok(None));
        // Every other reason is one of the offer's.
        for fault in [
            Unanswered::Unreadable,
            Unanswered::NoAudio,
            Unanswered::NoChannel,
            Unanswered::NoDialect,
        ] {
            assert_eq!(offered_again::<&str>(Err(fault)), Err(fault));
        }
    }

    #[test]
    fn a_name_stands_for_one_session_for_a_while_and_for_whoever_was_given_it() {
        let names = Names::default();
        let (mine, theirs) = ([1; 32], [2; 32]);
        let long = Duration::from_secs(60);
        names.keep("first".to_owned(), SessionId(7), mine, long);
        assert_eq!(names.names("first", &mine), Some(SessionId(7)));
        // Not for whoever says something else of who they are, and not
        // under another name.
        assert_eq!(names.names("first", &theirs), None);
        assert_eq!(names.names("second", &mine), None);
        assert_eq!(names.names("", &mine), None);
        // A session has one name: the one before stands for it no more.
        names.keep("second".to_owned(), SessionId(7), mine, long);
        assert_eq!(names.names("first", &mine), None);
        assert_eq!(names.names("second", &mine), Some(SessionId(7)));
        // Another session's name is another's.
        names.keep("third".to_owned(), SessionId(8), theirs, long);
        assert_eq!(names.names("second", &mine), Some(SessionId(7)));
        assert_eq!(names.names("third", &theirs), Some(SessionId(8)));
        // A name that is past its time stands for nothing, and is not
        // kept when the next is.
        names.keep("brief".to_owned(), SessionId(9), mine, Duration::ZERO);
        assert_eq!(names.names("brief", &mine), None);
        names.keep("fourth".to_owned(), SessionId(10), mine, long);
        let held = names.0.lock().unwrap();
        assert!(!held.contains_key("brief"));
        assert_eq!(held.len(), 3);
    }
}
