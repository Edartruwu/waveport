//! Calls over WebRTC: the offers that are posted, and what is said of
//! a call afterwards, in the shape of WHIP (RFC 9725).
//!
//! | Request | Answer |
//! |---|---|
//! | `POST /v1/webrtc`, an offer (`application/sdp`) | 201 and the answer, with where the call is (`Location`), the name of its checks (`ETag`) and a `Link` for each server the far end is told of |
//! | `PATCH /v1/webrtc/{call}`, a fragment (`application/trickle-ice-sdpfrag`) with `If-Match` | 204 where the fragment says where the far end is; 200 and a fragment, with another `ETag`, where its checks begin anew |
//! | `DELETE /v1/webrtc/{call}` | 200: the far end hung up |
//!
//! Who asks says a token, as `Authorization: Bearer`: the client token
//! of a session, or the token its client came back with. The offer is
//! answered for that session, and whatever is asked of the call later
//! is asked with the same token. The token is not used up by the
//! offer. It is used up, as on a WebSocket, by the hello the far end
//! says on its call, which names it again: so a far end whose call
//! never got through still holds a token it can come another way with.
//!
//! It is the shape of WHIP and not WHIP: audio goes both ways, and the
//! far end is a client of the protocol, which speaks on a data channel
//! that the offer has. An offer without one is turned away.
//!
//! The daemon answers the checks of a far end and makes none, so it is
//! heard from wherever a far end's checks come from. What a fragment
//! says of where the far end is, is not needed and not kept.

use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, to_bytes};
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_MATCH, LINK, LOCATION};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit as _, Mac as _};
use tokio::task::JoinHandle;
use waveport_adapter_kit::secret::Secret;
use waveport_assembly::{Failure, ResumeToken};
use waveport_ports::journal::SessionId;
use waveport_protocol::client::v1::{CLIENT_TOKEN, ErrorCode, RESUME_TOKEN};
use waveport_protocol::control::v1::Problem;
use waveport_transport_webrtc::{
    Answered, BindError, Client, Endpoint, Knock, Signals, Unanswered, fragment,
};

use crate::auth::{digest, same};
use crate::config;
use crate::daemon::{Shared, failed, raised};
use crate::http::{Refused, bearer, not_admitted, problem, said};
use crate::line::{self, Caller};

/// What an offer is, and an answer.
const SDP: &str = "application/sdp";

/// What a fragment is.
const FRAGMENT: &str = "application/trickle-ice-sdpfrag";

/// How many calls of one session may be on their way at once. A far
/// end that tries again makes another, and the one before it is still
/// waited for.
const OF_ONE_SESSION: usize = 4;

/// A server a far end is told of, and how it is told who it is there.
struct Server {
    urls: Vec<String>,
    known_as: KnownAs,
}

/// Who a far end says it is to a server it sends its call through.
enum KnownAs {
    /// Nobody: the server asks nobody.
    Nobody,
    /// A name and a password that are made for it, of a secret the
    /// server shares, and are good for a while.
    Made { secret: Secret, good_for: Duration },
    /// The name and the password every far end says.
    Fixed {
        username: String,
        credential: Secret,
    },
}

/// What is put between the quotes of a parameter of a header.
fn quoted(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Whether `value` can be said in a header at all.
fn is_sayable(value: &str) -> bool {
    value.bytes().all(|byte| (0x20..0x7f).contains(&byte))
}

impl Server {
    /// The server as the environment's variables, read by `read`, have
    /// what it is to keep to itself.
    fn held_by(
        server: &config::IceServer,
        read: &impl Fn(&str) -> Option<String>,
    ) -> Result<Self, Failure> {
        let held = |name: &str, what: &str| {
            let value = read(name).unwrap_or_default();
            let value = value.trim();
            if value.is_empty() || !is_sayable(value) {
                return Err(Failure(format!(
                    "the environment variable {name} holds no {what} that can be said, and \
                     `webrtc.ice_servers` names it"
                )));
            }
            Ok(Secret::new(value))
        };
        let known_as = match (&server.secret_env, &server.username, &server.credential_env) {
            (Some(name), ..) => KnownAs::Made {
                secret: held(name, "secret")?,
                good_for: Duration::from_secs(u64::from(server.ttl_s)),
            },
            (None, Some(username), Some(name)) => {
                if !is_sayable(username) {
                    return Err(Failure(
                        "a `username` of `webrtc.ice_servers` holds what no header can say"
                            .to_owned(),
                    ));
                }
                KnownAs::Fixed {
                    username: username.clone(),
                    credential: held(name, "password")?,
                }
            }
            _ => KnownAs::Nobody,
        };
        Ok(Self {
            urls: server.urls.clone(),
            known_as,
        })
    }

    /// What a far end of `session` is told of the server at `now`: a
    /// `Link` for each of its addresses (RFC 9725, section 4.6).
    fn links(&self, session: SessionId, now: SystemTime) -> Vec<HeaderValue> {
        let who = match &self.known_as {
            KnownAs::Nobody => String::new(),
            KnownAs::Made { secret, good_for } => {
                // A name that says until when it is good, and whose it
                // is, and a password that only whoever holds the secret
                // can make of it: base64(HMAC-SHA1(secret, name)).
                let until = now
                    .checked_add(*good_for)
                    .and_then(|until| until.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |until| until.as_secs());
                let username = format!("{until}:{session}");
                let Ok(mut mac) = Hmac::<sha1::Sha1>::new_from_slice(secret.expose().as_bytes())
                else {
                    return Vec::new();
                };
                mac.update(username.as_bytes());
                let credential = STANDARD.encode(mac.finalize().into_bytes());
                format!("; username=\"{username}\"; credential=\"{credential}\"")
            }
            KnownAs::Fixed {
                username,
                credential,
            } => format!(
                "; username=\"{}\"; credential=\"{}\"",
                quoted(username),
                quoted(credential.expose())
            ),
        };
        self.urls
            .iter()
            .filter_map(|url| {
                let mut link =
                    HeaderValue::from_str(&format!("<{url}>; rel=\"ice-server\"{who}")).ok()?;
                // It may hold a password.
                link.set_sensitive(!who.is_empty());
                Some(link)
            })
            .collect()
    }
}

/// A call, from its offer to its end.
struct Held {
    /// Whoever made the offer, as the hash of the token they said.
    by: [u8; 32],
    session: SessionId,
    /// What the far end says of the call here reaches it through this.
    signals: Signals,
    /// Which of the call's rounds of checks this is, from one.
    round: u64,
    /// The offer of the call, with the checks of this round.
    offer: String,
    /// Whether the far end has come.
    there: bool,
    /// Held by whoever has the call's checks begin anew, so that one
    /// does at a time.
    again: Arc<tokio::sync::Mutex<()>>,
}

/// What answers calls over WebRTC.
pub(crate) struct Webrtc {
    endpoint: Endpoint,
    servers: Vec<Server>,
    setting_up: usize,
    there_within: Duration,
    calls: Mutex<HashMap<String, Held>>,
    /// Whether what carries datagrams to their calls is gone.
    deaf: Arc<AtomicBool>,
    pump: JoinHandle<()>,
}

impl Drop for Webrtc {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

/// The name of a new call: nothing that can be guessed, since whoever
/// names a call may ask about it.
fn name() -> Result<String, Problem> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|_| Problem::of(ErrorCode::Internal, "no name could be made for the call"))?;
    Ok(format!("{:032x}", u128::from_be_bytes(random)))
}

/// The name of a round of checks, as `ETag` says it.
fn tag(round: u64) -> String {
    format!("\"{round}\"")
}

/// Whether what `If-Match` says holds for the round `round`.
fn holds(if_match: &str, round: u64) -> bool {
    let current = tag(round);
    if_match.trim() == "*" || if_match.split(',').any(|named| named.trim() == current)
}

fn unanswered(unanswered: Unanswered) -> Problem {
    match unanswered {
        Unanswered::Unreadable => Problem::of(
            ErrorCode::Malformed,
            "the body is no offer that can be answered",
        ),
        Unanswered::NoAudio => Problem::of(
            ErrorCode::Unsupported,
            "the offer asks for no audio, and a call is one of audio",
        ),
        Unanswered::NoChannel => Problem::of(
            ErrorCode::Unsupported,
            "the offer has no data channel: a client speaks the protocol on one, which it \
             has before it offers",
        ),
        Unanswered::Over => no_call(),
    }
}

fn no_call() -> Problem {
    Problem::of(
        ErrorCode::NoSession,
        "there is no such call of whoever asks",
    )
}

impl Webrtc {
    /// Sets up the sockets calls are taken at, and has their datagrams
    /// carried.
    ///
    /// # Errors
    ///
    /// The system gave no socket, or a variable that is to hold what a
    /// server is told holds nothing.
    pub(crate) async fn start(config: &config::Webrtc) -> Result<Self, Failure> {
        Self::held_by(config, |name| std::env::var(name).ok()).await
    }

    /// The same, with the variables as `read` reads them.
    async fn held_by(
        config: &config::Webrtc,
        read: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, Failure> {
        let mut servers = Vec::new();
        for server in &config.ice_servers {
            servers.push(Server::held_by(server, &read)?);
        }
        let endpoint = waveport_transport_webrtc::Config {
            udp: config.udp,
            tcp: config.tcp,
            reached_at: config.reached_at,
            dtls13: config.dtls13,
        };
        let (endpoint, pump) = Endpoint::bind(endpoint).await.map_err(|error| {
            Failure(match error {
                BindError::System(error) => format!(
                    "calls over WebRTC cannot be taken at {}: {error}",
                    config.udp
                ),
                other => format!("calls over WebRTC cannot be taken: {other}"),
            })
        })?;
        let deaf = Arc::new(AtomicBool::new(false));
        let gone = Arc::clone(&deaf);
        let pump = tokio::spawn(async move {
            let error = pump.run().await;
            gone.store(true, Ordering::Relaxed);
            eprintln!(
                "waveportd: the socket of the calls over WebRTC failed, and no call is \
                 answered any more: {error}"
            );
        });
        Ok(Self {
            endpoint,
            servers,
            setting_up: config.setting_up,
            there_within: Duration::from_millis(u64::from(config.there_within_ms)),
            calls: Mutex::default(),
            deaf,
            pump,
        })
    }

    /// What whoever runs the daemon should know of it.
    pub(crate) fn note(&self) -> String {
        let connections = self
            .endpoint
            .tcp()
            .map_or_else(String::new, |tcp| format!(", connections at {tcp}"));
        format!(
            "calls over WebRTC: far ends are told to send datagrams to {}{connections}",
            self.endpoint.udp()
        )
    }

    fn calls(&self) -> MutexGuard<'_, HashMap<String, Held>> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What a far end of `session` is told of the servers.
    fn links(&self, session: SessionId) -> Vec<HeaderValue> {
        let now = SystemTime::now();
        self.servers
            .iter()
            .flat_map(|server| server.links(session, now))
            .collect()
    }

    /// Answers `offer` for `session`, made by whoever said the token
    /// whose hash is `by`. Returns the name of the call, the answer,
    /// and the call, which has not begun.
    fn answer(
        &self,
        session: SessionId,
        by: [u8; 32],
        offer: &str,
    ) -> Result<(String, String, Knock), Problem> {
        if self.deaf.load(Ordering::Relaxed) {
            return Err(Problem::of(
                ErrorCode::Internal,
                "calls over WebRTC are not answered any more: their socket failed",
            ));
        }
        let call = name()?;
        let mut calls = self.calls();
        let coming = || calls.values().filter(|call| !call.there);
        let of_this = coming().filter(|call| call.session == session).count();
        if coming().count() >= self.setting_up || of_this >= OF_ONE_SESSION {
            return Err(Problem::of(
                ErrorCode::Busy,
                "as many calls are on their way as may be: the offer can be made again in a \
                 moment",
            ));
        }
        let Answered {
            answer,
            knock,
            signals,
        } = self.endpoint.offer(offer, Client::V1).map_err(unanswered)?;
        let held = Held {
            by,
            session,
            signals,
            round: 1,
            offer: offer.to_owned(),
            there: false,
            again: Arc::default(),
        };
        calls.insert(call.clone(), held);
        Ok((call, answer, knock))
    }

    /// The call `call`, for whoever made its offer: they say the token
    /// they made it with, whose hash is `by`.
    fn held<T>(&self, call: &str, by: &[u8; 32], of: impl FnOnce(&mut Held) -> T) -> Option<T> {
        let mut calls = self.calls();
        let held = calls.get_mut(call).filter(|held| same(&held.by, by))?;
        Some(of(held))
    }
}

/// What lets go of a call when whoever served it is done.
struct Forgets {
    daemon: Arc<Shared>,
    call: String,
}

impl Drop for Forgets {
    fn drop(&mut self) {
        if let Some(webrtc) = &self.daemon.webrtc {
            webrtc.calls().remove(&self.call);
        }
    }
}

/// What answers calls, for a request that asks about one.
fn answering(daemon: &Shared) -> Result<&Webrtc, Problem> {
    daemon.webrtc.as_ref().ok_or_else(|| {
        problem(
            StatusCode::NOT_FOUND,
            ErrorCode::Unsupported,
            "the daemon answers no calls over WebRTC: `webrtc` in its configuration says where \
             it would",
        )
    })
}

/// Whether a request says that what it carries is of the kind `kind`.
fn is_of_kind(headers: &HeaderMap, kind: &str) -> bool {
    said(headers, CONTENT_TYPE)
        .and_then(|said| said.split(';').next())
        .is_some_and(|said| said.trim().eq_ignore_ascii_case(kind))
}

/// What a request carries, as text.
async fn text(daemon: &Shared, body: Body) -> Result<String, Problem> {
    let Ok(body) = to_bytes(body, daemon.limits.body_bytes).await else {
        return Err(problem(
            StatusCode::PAYLOAD_TOO_LARGE,
            ErrorCode::Malformed,
            "the body is larger than a body may be, or did not arrive",
        ));
    };
    String::from_utf8(body.to_vec())
        .map_err(|_| Problem::of(ErrorCode::Malformed, "the body is no text"))
}

/// The session whoever asks holds a token of, and the hash of the
/// token. A client token is looked at and not used up.
async fn asks(daemon: &Shared, headers: &HeaderMap) -> Result<(SessionId, [u8; 32]), Problem> {
    let none = || {
        not_admitted(
            "a call is asked about by whoever holds a client token of its session, or the token \
             its client came back with, said as `Authorization: Bearer`",
        )
    };
    let token = bearer(headers)?.ok_or_else(none)?;
    let session = if token.starts_with(CLIENT_TOKEN) {
        daemon.tickets.names(token).ok_or_else(|| {
            not_admitted("this is no client token: it was used, is too old, or was never one")
        })?
    } else if token.starts_with(RESUME_TOKEN) {
        let no = || not_admitted("this is no resume token of a session");
        let token: ResumeToken = token.parse().map_err(|_| no())?;
        let admitted = daemon
            .stack
            .kept
            .admits(&token)
            .await
            .map_err(|failure| failed("the token could not be looked at", &failure))?;
        if !admitted {
            return Err(no());
        }
        token.session()
    } else {
        return Err(none());
    };
    Ok((session, digest(token)))
}

/// The hash of the token whoever asks about a call says. It is not
/// looked at here: it is the one the call's offer was made with, or the
/// call is not theirs.
fn says(headers: &HeaderMap) -> Result<[u8; 32], Problem> {
    bearer(headers)?
        .map(digest)
        .ok_or_else(|| not_admitted("a call is asked about with the token its offer was made with"))
}

/// An answer with nothing in it, which says what may be sent where it
/// was asked.
fn takes(name: &'static str, kind: &'static str) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static(name),
        HeaderValue::from_static(kind),
    );
    response
}

/// Answers whoever asks what is taken where offers are posted. One who
/// says a token is told of the servers too, for a far end that has to
/// know them before it can offer.
pub(crate) async fn offers(State(daemon): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    let mut response = takes("accept-post", SDP);
    if let (Ok(webrtc), Ok((session, _))) = (answering(&daemon), asks(&daemon, &headers).await) {
        for link in webrtc.links(session) {
            response.headers_mut().append(LINK, link);
        }
        let none = HeaderValue::from_static("no-store");
        response.headers_mut().insert(CACHE_CONTROL, none);
    }
    response
}

/// Answers whoever asks for what is where offers are posted: nothing is
/// (RFC 9725, section 4.1).
pub(crate) async fn nothing(State(daemon): State<Arc<Shared>>) -> Result<Response, Refused> {
    answering(&daemon)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Answers an offer: the call is set up, and waited for.
pub(crate) async fn offer(
    State(daemon): State<Arc<Shared>>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, Refused> {
    if daemon.is_draining() {
        return Err(Shared::busy().into());
    }
    let webrtc = answering(&daemon)?;
    if !is_of_kind(&headers, SDP) {
        return Err(problem(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ErrorCode::Unsupported,
            "an offer is SDP, and says so: `Content-Type: application/sdp`",
        )
        .into());
    }
    let (session, by) = asks(&daemon, &headers).await?;
    daemon.takes_a_call(session)?;
    let offer = text(&daemon, body).await?;
    // A description says first which version it is of, and there is
    // one (RFC 8866, section 5.1). What does not is no description, and
    // is not looked at for what it asks.
    if !offer.starts_with("v=0") {
        return Err(unanswered(Unanswered::Unreadable).into());
    }
    let (call, answer, knock) = webrtc.answer(session, by, &offer)?;
    let mut response = (StatusCode::CREATED, answer).into_response();
    let said = response.headers_mut();
    said.insert(CONTENT_TYPE, HeaderValue::from_static(SDP));
    // It holds what the checks of the call are made with.
    said.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(at) = HeaderValue::from_str(&format!("/v1/webrtc/{call}")) {
        said.insert(LOCATION, at);
    }
    if let Ok(round) = HeaderValue::from_str(&tag(1)) {
        said.insert(ETAG, round);
    }
    for link in webrtc.links(session) {
        said.append(LINK, link);
    }
    tokio::spawn(serve(Arc::clone(&daemon), call, session, knock));
    Ok(response)
}

/// One call, from the answer to its offer to its end.
async fn serve(daemon: Arc<Shared>, call: String, session: SessionId, knock: Knock) {
    let _forgets = Forgets {
        daemon: Arc::clone(&daemon),
        call: call.clone(),
    };
    let Some(webrtc) = &daemon.webrtc else {
        return;
    };
    let mut going = daemon.going();
    let came = tokio::select! {
        came = knock.connect(webrtc.there_within) => came,
        // A daemon that is going away waits for nobody. A call that is
        // let go of is ended.
        () = raised(&mut going) => return,
    };
    // One that did not come, or did not begin as a client does, has
    // been told so where it could be.
    let Ok(caller) = came else {
        return;
    };
    if let Some(held) = webrtc.calls().get_mut(&call) {
        held.there = true;
    }
    let Some(hello) = caller.hello().cloned() else {
        // A client of the protocol has said hello by now.
        caller
            .refuse(ErrorCode::OutOfPlace, "a client says hello")
            .await;
        return;
    };
    let caller = Caller::Call(Box::new(caller), hello);
    line::seated(&daemon, caller, Some(session)).await;
}

/// Answers whoever asks whether a call is there: with nothing, if it
/// is theirs.
pub(crate) async fn there(
    State(daemon): State<Arc<Shared>>,
    Path(call): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Refused> {
    let webrtc = answering(&daemon)?;
    let by = says(&headers)?;
    webrtc.held(&call, &by, |_| ()).ok_or_else(no_call)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Answers whoever asks what is taken where a call is.
pub(crate) async fn fragments() -> Response {
    takes("accept-patch", FRAGMENT)
}

/// Takes what a far end says of its checks: where it is, or that they
/// begin anew.
pub(crate) async fn again(
    State(daemon): State<Arc<Shared>>,
    Path(call): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, Refused> {
    let webrtc = answering(&daemon)?;
    let by = says(&headers)?;
    if !is_of_kind(&headers, FRAGMENT) {
        return Err(problem(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ErrorCode::Unsupported,
            "what is said of a call's checks is a fragment, and says so: `Content-Type: \
             application/trickle-ice-sdpfrag`",
        )
        .into());
    }
    let one = webrtc
        .held(&call, &by, |held| Arc::clone(&held.again))
        .ok_or_else(no_call)?;
    let sent = text(&daemon, body).await?;
    let Some(checks) = fragment::credentials(&sent) else {
        return Err(Problem::of(
            ErrorCode::Malformed,
            "the body names no checks: a fragment says their name and their password \
             (`a=ice-ufrag`, `a=ice-pwd`)",
        )
        .into());
    };
    // A fragment is for one round of checks, which it names, or for
    // whichever there is.
    let Some(if_match) = said(&headers, IF_MATCH) else {
        return Err(problem(
            StatusCode::PRECONDITION_REQUIRED,
            ErrorCode::Malformed,
            "a fragment says which of the call's checks it is for: `If-Match`, with the `ETag` \
             the call was answered with, or `*` where they begin anew",
        )
        .into());
    };
    let _one = one.lock().await;
    let (round, offer, signals) = webrtc
        .held(&call, &by, |held| {
            (held.round, held.offer.clone(), held.signals.clone())
        })
        .ok_or_else(no_call)?;
    if !holds(if_match, round) {
        return Err(problem(
            StatusCode::PRECONDITION_FAILED,
            ErrorCode::OutOfPlace,
            "the fragment is for other checks than the call has now",
        )
        .into());
    }
    if fragment::credentials(&offer) == Some(checks) {
        // Where the far end is. It is heard from wherever its checks
        // come from, so that is not needed, and not kept.
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    // Other checks than the call has: they begin anew. The offer the
    // call began with, made with them, is answered by the call.
    let cannot = || {
        Problem::of(
            ErrorCode::Unsupported,
            "the call's checks could not be begun anew with what the fragment says: the call \
             goes on as it was",
        )
    };
    let offered = fragment::into(&offer, &sent).ok_or_else(cannot)?;
    let answered = match signals.offer(&offered).await {
        Ok(answered) => answered,
        Err(Unanswered::Over) => return Err(no_call().into()),
        Err(_) => return Err(cannot().into()),
    };
    let back = fragment::of(&answered)
        .ok_or_else(|| Problem::of(ErrorCode::Internal, "the answer says nothing of its checks"))?;
    webrtc
        .held(&call, &by, |held| {
            held.round = round + 1;
            held.offer = offered;
        })
        .ok_or_else(no_call)?;
    let mut response = (StatusCode::OK, back).into_response();
    let said = response.headers_mut();
    said.insert(CONTENT_TYPE, HeaderValue::from_static(FRAGMENT));
    said.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(round) = HeaderValue::from_str(&tag(round + 1)) {
        said.insert(ETAG, round);
    }
    Ok(response)
}

/// The far end hung up, where it made its offer.
pub(crate) async fn ends(
    State(daemon): State<Arc<Shared>>,
    Path(call): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Refused> {
    let webrtc = answering(&daemon)?;
    let by = says(&headers)?;
    let signals = {
        let mut calls = webrtc.calls();
        let theirs = calls.get(&call).is_some_and(|held| same(&held.by, &by));
        theirs.then(|| calls.remove(&call)).flatten()
    }
    .map(|held| held.signals)
    .ok_or_else(no_call)?;
    signals.end().await;
    Ok(StatusCode::OK.into_response())
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::time::UNIX_EPOCH;

    use waveport_ports::journal::SessionId;

    use super::{Server, holds, quoted, tag};
    use crate::config::IceServer;

    fn server(urls: &[&str]) -> IceServer {
        IceServer {
            urls: urls.iter().map(|url| (*url).to_owned()).collect(),
            secret_env: None,
            ttl_s: 3_600,
            username: None,
            credential_env: None,
        }
    }

    /// What the links of `server` say, with the variables as they are
    /// here.
    fn links(server: &IceServer, session: u128, at: u64) -> Vec<String> {
        let read = |name: &str| match name {
            "TURN_SECRET" => Some("north".to_owned()),
            "TURN_PASSWORD" => Some("a \"word\\".to_owned()),
            _ => None,
        };
        let relays = server.relays();
        let server = Server::held_by(server, &read).unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(at);
        server
            .links(SessionId(session), now)
            .iter()
            .map(|link| {
                // What holds a password is not shown by whatever prints
                // an answer, and what holds none is.
                assert_eq!(link.is_sensitive(), relays);
                link.to_str().unwrap().to_owned()
            })
            .collect()
    }

    #[test]
    fn a_far_end_is_told_of_a_server_as_a_link_says_it() {
        // A server that asks nobody who they are.
        let stun = server(&["stun:stun.example.net", "stun:stun.example.net:3479"]);
        assert_eq!(
            links(&stun, 7, 1_000),
            [
                "<stun:stun.example.net>; rel=\"ice-server\"",
                "<stun:stun.example.net:3479>; rel=\"ice-server\"",
            ]
        );

        // One that every far end says the same name and password to:
        // what would end the quotes it stands between is said so that
        // it does not.
        let fixed = IceServer {
            username: Some("every\"one".to_owned()),
            credential_env: Some("TURN_PASSWORD".to_owned()),
            ..server(&["turn:turn.example.net?transport=udp"])
        };
        assert_eq!(
            links(&fixed, 7, 1_000),
            [
                "<turn:turn.example.net?transport=udp>; rel=\"ice-server\"; \
              username=\"every\\\"one\"; credential=\"a \\\"word\\\\\""
            ]
        );
        assert_eq!(quoted("plain"), "plain");
    }

    #[test]
    fn a_far_end_is_made_a_name_and_a_password_that_are_good_for_a_while() {
        let made = IceServer {
            secret_env: Some("TURN_SECRET".to_owned()),
            ttl_s: 600,
            ..server(&[
                "turn:turn.example.net",
                "turns:turn.example.net?transport=tcp",
            ])
        };
        let said = links(&made, 7, 1_000);
        // The name says until when it is good and whose it is. The
        // password is base64(HMAC-SHA1(secret, name)), which is what a
        // server that shares the secret makes of the name itself: for
        // the secret `north` and this name, it is this.
        let who = format!(
            "; rel=\"ice-server\"; username=\"1600:{}\"; \
             credential=\"{}\"",
            SessionId(7),
            password("north", &format!("1600:{}", SessionId(7)))
        );
        assert_eq!(
            said,
            [
                format!("<turn:turn.example.net>{who}"),
                format!("<turns:turn.example.net?transport=tcp>{who}"),
            ]
        );
        // Another session, and another moment, have another name and
        // another password.
        let other = links(&made, 8, 1_000);
        let later = links(&made, 7, 1_001);
        assert_ne!(other, said);
        assert_ne!(later, said);
        assert!(later[0].contains("username=\"1601:"), "{later:?}");
    }

    /// base64(HMAC-SHA1(secret, name)), from the definition of HMAC
    /// (RFC 2104) and nothing this crate uses to make one.
    fn password(secret: &str, name: &str) -> String {
        use base64::Engine as _;
        use sha1::Digest as _;
        let mut key = [0_u8; 64];
        key[..secret.len()].copy_from_slice(secret.as_bytes());
        let inner: Vec<u8> = key.iter().map(|byte| byte ^ 0x36).collect();
        let outer: Vec<u8> = key.iter().map(|byte| byte ^ 0x5c).collect();
        let first = sha1::Sha1::new()
            .chain_update(&inner)
            .chain_update(name.as_bytes())
            .finalize();
        let second = sha1::Sha1::new()
            .chain_update(&outer)
            .chain_update(first)
            .finalize();
        base64::engine::general_purpose::STANDARD.encode(second)
    }

    #[test]
    fn a_server_whose_secret_is_not_there_is_not_told_of() {
        let read = |_: &str| None;
        let made = IceServer {
            secret_env: Some("TURN_SECRET".to_owned()),
            ..server(&["turn:turn.example.net"])
        };
        let failure = Server::held_by(&made, &read).err().unwrap();
        assert!(failure.0.contains("TURN_SECRET"), "{failure}");
        // What no header can say is no password either, and is not
        // repeated.
        let read = |_: &str| Some("north\u{7f}south".to_owned());
        let fixed = IceServer {
            username: Some("everyone".to_owned()),
            credential_env: Some("TURN_PASSWORD".to_owned()),
            ..server(&["turn:turn.example.net"])
        };
        let failure = Server::held_by(&fixed, &read).err().unwrap();
        assert!(failure.0.contains("TURN_PASSWORD"), "{failure}");
        assert!(!failure.0.contains("north"), "{failure}");
    }

    #[test]
    fn a_fragment_is_for_the_checks_it_names_or_for_whichever_there_are() {
        assert_eq!(tag(1), "\"1\"");
        assert!(holds("\"1\"", 1));
        assert!(holds(" \"1\" ", 1));
        assert!(holds("*", 7));
        assert!(holds("\"3\", \"4\"", 4));
        assert!(!holds("\"1\"", 2));
        assert!(!holds("\"11\"", 1));
        // A tag that is only alike is not the same.
        assert!(!holds("W/\"1\"", 1));
        assert!(!holds("1", 1));
        assert!(!holds("", 1));
    }
}
