//! What the daemon is asked over HTTP, and what it answers.
//!
//! | Request | Who may ask | Answer |
//! |---|---|---|
//! | `POST /v1/sessions` | a control key | 201, a session and a client token |
//! | `GET /v1/sessions` | a control key | the sessions; `?state=broken` the ones to go on with |
//! | `GET /v1/sessions/{id}` | a control key | the session |
//! | `DELETE /v1/sessions/{id}` | a control key | 204 when it is gone, 202 when it was asked to end |
//! | `POST /v1/sessions/{id}/tokens` | a control key | 201, another client token |
//! | `POST /v1/sessions/{id}/input` | a control key, or the session's resume token | 202 |
//! | `GET /v1/sessions/{id}/events` | a control key, a client token, or the session's resume token | a stream of server-sent events |
//! | `GET /healthz` | anyone | 200, or 503 while the daemon stops |
//!
//! A body is JSON, and says so (`Content-Type: application/json`). What
//! is refused is answered with a problem (RFC 9457).

use core::convert::Infallible;
use core::time::Duration;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_EXPOSE_HEADERS, ACCESS_CONTROL_MAX_AGE, ACCESS_CONTROL_REQUEST_METHOD,
    AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, HOST, LOCATION, ORIGIN, RETRY_AFTER, VARY,
    WWW_AUTHENTICATE,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::Sse;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::stream;
use serde::Serialize;
use serde::de::DeserializeOwned;
use waveport_assembly::ResumeToken;
use waveport_domain::event::{HandoffContext, Key};
use waveport_domain::ids::{AgentId, ParticipantId};
use waveport_ports::control::{ControlError, SessionInput};
use waveport_ports::journal::SessionId;
use waveport_protocol::client::v1::{
    self as client, CLIENT_TOKEN, ErrorCode, FromClient, FromServer, RESUME_TOKEN, Refusal,
};
use waveport_protocol::control::v1::{Context, CreateSession, Input, Problem, SessionState};

use crate::auth::From;
use crate::daemon::{Shared, failed};
use crate::events::Following;
use waveport_assembly::shown::{Asked, Position};

/// The daemon, for a request.
type Daemon = State<Arc<Shared>>;

/// Who typed, when nobody says who: the session's first participant.
const WRITER: ParticipantId = ParticipantId(0);

/// A problem, as the answer to a request.
pub(crate) struct Refused(Problem);

impl core::convert::From<Problem> for Refused {
    fn from(problem: Problem) -> Self {
        Self(problem)
    }
}

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        let Self(problem) = self;
        let status = StatusCode::from_u16(problem.status).unwrap_or(StatusCode::BAD_REQUEST);
        let mut response = written(status, "application/problem+json", &problem);
        let headers = response.headers_mut();
        if status == StatusCode::UNAUTHORIZED {
            headers.insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        if status == StatusCode::SERVICE_UNAVAILABLE {
            headers.insert(RETRY_AFTER, HeaderValue::from_static("1"));
        }
        response
    }
}

/// An answer with a body of JSON.
fn written(status: StatusCode, kind: &'static str, body: &impl Serialize) -> Response {
    let body = serde_json::to_vec(body).unwrap_or_default();
    let mut response = (status, body).into_response();
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(kind));
    // What is said of a session is for whoever asked, and nobody
    // between.
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn json(status: StatusCode, body: &impl Serialize) -> Response {
    written(status, "application/json", body)
}

/// A problem that has a status of its own.
fn problem(status: StatusCode, code: ErrorCode, detail: &str) -> Problem {
    Problem {
        status: status.as_u16(),
        title: status.canonical_reason().unwrap_or_default().to_owned(),
        code,
        detail: Some(detail.to_owned()),
    }
}

fn forbidden(detail: &str) -> Response {
    Refused(Problem::of(ErrorCode::Forbidden, detail)).into_response()
}

fn not_admitted(detail: &str) -> Problem {
    Problem::of(ErrorCode::Token, detail)
}

fn refusal(refusal: Refusal) -> Problem {
    Problem::of(refusal.code(), refusal.to_string())
}

/// What a header says, if it is there and is text.
fn said(headers: &HeaderMap, name: impl axum::http::header::AsHeaderName) -> Option<&str> {
    headers.get(name)?.to_str().ok()
}

/// The routes of the daemon.
pub(crate) fn router(daemon: &Arc<Shared>) -> Router {
    let asked = Router::new()
        .route("/v1/sessions", post(create).get(list))
        .route("/v1/sessions/{id}", get(info).delete(end))
        .route("/v1/sessions/{id}/tokens", post(mint))
        .route("/v1/sessions/{id}/input", post(input))
        .route("/v1/sessions/{id}/events", get(events))
        .fallback(nowhere)
        .layer(middleware::from_fn_with_state(Arc::clone(daemon), gate));
    Router::new()
        // Whoever looks after the daemon asks by its address, and is
        // told nothing that is anyone's.
        .route("/healthz", get(health))
        .merge(asked)
        .with_state(Arc::clone(daemon))
}

/// Looks at where a request says it is going and where it comes from,
/// before anything is done for it, and gives it no longer than a
/// request may take.
async fn gate(State(daemon): Daemon, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let host = said(headers, HOST);
    if !daemon.gate.is_here(host) {
        return forbidden(
            "the request names a host the daemon is not reached by: `hosts` in its \
             configuration names the ones it is",
        );
    }
    let from = daemon.gate.from(said(headers, ORIGIN), host);
    // A browser says how a request relates to the page that makes it,
    // also where it says no origin: an image or a link on a page from
    // elsewhere is a request, too.
    let elsewhere = match (from, said(headers, "sec-fetch-site")) {
        (From::Elsewhere, _) | (From::Nowhere, Some("cross-site" | "same-site")) => true,
        (From::Nowhere | From::Here | From::Allowed, _) => false,
    };
    if elsewhere {
        return forbidden(
            "the request comes from a page the daemon was not told of: `allowed_origins` in \
             its configuration names the ones that may ask",
        );
    }
    let allowed = (from == From::Allowed)
        .then(|| headers.get(ORIGIN).cloned())
        .flatten();
    // A browser asks first whether a page from elsewhere may ask at all.
    let asks_first =
        request.method() == Method::OPTIONS && headers.contains_key(ACCESS_CONTROL_REQUEST_METHOD);
    let mut response = if asks_first && allowed.is_some() {
        let mut response = StatusCode::NO_CONTENT.into_response();
        let headers = response.headers_mut();
        let methods = HeaderValue::from_static("GET, POST, DELETE");
        headers.insert(ACCESS_CONTROL_ALLOW_METHODS, methods);
        let asked_with = HeaderValue::from_static("authorization, content-type, last-event-id");
        headers.insert(ACCESS_CONTROL_ALLOW_HEADERS, asked_with);
        headers.insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
        response
    } else {
        let within = Duration::from_millis(u64::from(daemon.limits.request_ms));
        match tokio::time::timeout(within, next.run(request)).await {
            Ok(response) => response,
            Err(_) => Refused(problem(
                StatusCode::REQUEST_TIMEOUT,
                ErrorCode::Busy,
                "the request took longer than a request may",
            ))
            .into_response(),
        }
    };
    if let Some(origin) = allowed {
        let headers = response.headers_mut();
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        headers.insert(VARY, HeaderValue::from_static("Origin"));
        let shown = HeaderValue::from_static("Location, Retry-After");
        headers.insert(ACCESS_CONTROL_EXPOSE_HEADERS, shown);
    }
    response
}

async fn nowhere() -> Refused {
    Refused(problem(
        StatusCode::NOT_FOUND,
        ErrorCode::UnknownMessage,
        "nothing is asked at this address",
    ))
}

async fn health(State(daemon): Daemon) -> Response {
    #[derive(Serialize)]
    struct Health {
        status: &'static str,
    }
    if daemon.is_draining() {
        let status = "stopping";
        json(StatusCode::SERVICE_UNAVAILABLE, &Health { status })
    } else {
        json(StatusCode::OK, &Health { status: "ok" })
    }
}

/// What follows `Bearer` in the request, if it says who asks.
fn bearer(headers: &HeaderMap) -> Result<Option<&str>, Problem> {
    let Some(written) = headers.get(AUTHORIZATION) else {
        return Ok(None);
    };
    let token = written
        .to_str()
        .ok()
        .and_then(|written| written.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim())
        .filter(|token| !token.is_empty());
    token
        .map(Some)
        .ok_or_else(|| not_admitted("who asks is said as `Authorization: Bearer`, and a token"))
}

/// Whether whoever asks holds a control key. A daemon that has none
/// lets in whoever says nothing of who they are.
fn control(daemon: &Shared, headers: &HeaderMap) -> Result<(), Problem> {
    match bearer(headers)? {
        None if daemon.keys.is_empty() => Ok(()),
        None => Err(not_admitted("a control key is needed to ask this")),
        Some(key) if daemon.keys.admit(key) => Ok(()),
        Some(_) if daemon.keys.is_empty() => Err(not_admitted(
            "the daemon has no control key, and this is none: ask without one",
        )),
        Some(_) => Err(not_admitted("this is no control key of the daemon")),
    }
}

/// Who asks something of one session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Who {
    /// Whoever holds a control key.
    Control,
    /// The far end of the session, which has just connected with a
    /// client token.
    Connected,
    /// The far end of the session, which holds its resume token.
    Client,
}

/// Who asks something of the session `id`. A client token is taken, and
/// used up, only where `connects` says that this is the request a far
/// end connects with.
async fn who(
    daemon: &Shared,
    headers: &HeaderMap,
    id: SessionId,
    connects: bool,
) -> Result<Who, Problem> {
    let Some(token) = bearer(headers)? else {
        return control(daemon, headers).map(|()| Who::Control);
    };
    if token.starts_with(CLIENT_TOKEN) {
        if !connects {
            return Err(not_admitted(
                "a client token connects to a session, and asks nothing else of it",
            ));
        }
        return if daemon.tickets.redeem(token, id) {
            Ok(Who::Connected)
        } else {
            Err(not_admitted(
                "this is no client token of the session: it was used, is too old, or was \
                 never one",
            ))
        };
    }
    if token.starts_with(RESUME_TOKEN) {
        let none = || not_admitted("this is no resume token of the session");
        let token: ResumeToken = token.parse().map_err(|_| none())?;
        let admitted = daemon
            .stack
            .kept
            .admits(&token)
            .await
            .map_err(|failure| failed("the token could not be looked at", &failure))?;
        return if admitted && token.session() == id {
            Ok(Who::Client)
        } else {
            Err(none())
        };
    }
    control(daemon, headers).map(|()| Who::Control)
}

/// The session a path names.
fn session(written: &str) -> Result<SessionId, Problem> {
    let plain = written.len() == 32 && written.bytes().all(|byte| byte.is_ascii_hexdigit());
    plain
        .then(|| u128::from_str_radix(written, 16).ok())
        .flatten()
        .map(SessionId)
        .ok_or_else(|| Problem::of(ErrorCode::NoSession, "there is no session of that name"))
}

/// The body of a request, as the JSON of a `T`.
async fn read<T: DeserializeOwned>(
    daemon: &Shared,
    headers: &HeaderMap,
    body: Body,
) -> Result<T, Problem> {
    // A page from elsewhere cannot say this without asking first.
    let json = said(headers, CONTENT_TYPE)
        .and_then(|kind| kind.split(';').next())
        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("application/json"));
    if !json {
        return Err(problem(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ErrorCode::Unsupported,
            "a body is JSON, and says so: `Content-Type: application/json`",
        ));
    }
    let most = daemon.limits.body_bytes;
    let Ok(body) = to_bytes(body, most).await else {
        return Err(problem(
            StatusCode::PAYLOAD_TOO_LARGE,
            ErrorCode::Malformed,
            "the body is larger than a body may be, or did not arrive",
        ));
    };
    serde_json::from_slice(&body).map_err(|error| {
        // What is wrong and where, as the parser says it. The body is
        // whoever asked's own.
        Problem::of(
            ErrorCode::Malformed,
            format!("the body cannot be read: {error}"),
        )
    })
}

/// The values a query gives `name`: each time it is named, and each
/// part between commas.
fn values<'a>(query: Option<&'a str>, name: &'a str) -> impl Iterator<Item = String> + 'a {
    query
        .unwrap_or_default()
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(move |(named, _)| *named == name)
        .flat_map(|(_, value)| {
            let value = value.replace("%2C", ",").replace("%2c", ",");
            let parts: Vec<String> = value.split(',').map(str::to_owned).collect();
            parts
        })
        .filter(|value| !value.is_empty())
}

async fn create(
    State(daemon): Daemon,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, Refused> {
    control(&daemon, &headers)?;
    let asked: CreateSession = read(&daemon, &headers, body).await?;
    let created = daemon.create(&asked.agent)?;
    let mut response = json(StatusCode::CREATED, &created);
    if let Ok(at) = HeaderValue::from_str(&format!("/v1/sessions/{}", created.session)) {
        response.headers_mut().insert(LOCATION, at);
    }
    Ok(response)
}

async fn list(
    State(daemon): Daemon,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Result<Response, Refused> {
    control(&daemon, &headers)?;
    let state = match values(query.as_deref(), "state").next() {
        None => None,
        Some(named) => {
            let state = serde_json::from_value(serde_json::Value::String(named));
            match state {
                Ok(SessionState::Unknown) | Err(_) => {
                    let detail = "a session does not stand as `state` says";
                    return Err(Problem::of(ErrorCode::Malformed, detail).into());
                }
                Ok(state) => Some(state),
            }
        }
    };
    Ok(json(StatusCode::OK, &daemon.list(state)))
}

async fn info(
    State(daemon): Daemon,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Refused> {
    control(&daemon, &headers)?;
    let info = daemon.info(session(&id)?)?;
    Ok(json(StatusCode::OK, &info))
}

async fn end(
    State(daemon): Daemon,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Refused> {
    control(&daemon, &headers)?;
    let gone = daemon.end(session(&id)?).await?;
    let status = if gone {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::ACCEPTED
    };
    Ok(status.into_response())
}

async fn mint(
    State(daemon): Daemon,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Refused> {
    control(&daemon, &headers)?;
    let minted = daemon.mint(session(&id)?)?;
    Ok(json(StatusCode::CREATED, &minted))
}

/// What an application asks of a session, as the session takes it.
fn asked_by_control(asked: Input) -> Result<SessionInput, Problem> {
    asked.check().map_err(refusal)?;
    Ok(match asked {
        Input::Text { text, who } => SessionInput::Text {
            who: who.map_or(WRITER, ParticipantId),
            text,
        },
        Input::Say { text } => SessionInput::Say { text },
        Input::Press { keys } => SessionInput::Press {
            keys: keys.chars().filter_map(Key::from_char).collect(),
        },
        Input::Interrupt => SessionInput::Interrupt,
        Input::Handoff { to, context } => SessionInput::Handoff {
            to: AgentId(to),
            context: match context {
                Context::Full => HandoffContext::Full,
                Context::Summary(summary) => HandoffContext::Summary(summary.into()),
                Context::None => HandoffContext::None,
            },
        },
        Input::Close => SessionInput::Close,
        Input::Unknown => return Err(refusal(Refusal::Unknown)),
    })
}

/// What the far end of a session says, as the session takes it. Over
/// this route it says what is not said with a connection: what it
/// typed, that the agent is to stop, and that it leaves.
fn said_by_client(said: FromClient) -> Result<SessionInput, Problem> {
    said.check().map_err(refusal)?;
    match said {
        FromClient::Text { text } => Ok(SessionInput::Text { who: WRITER, text }),
        FromClient::Interrupt => Ok(SessionInput::Interrupt),
        FromClient::Bye => Ok(SessionInput::Close),
        FromClient::Dtmf { .. } => Err(Problem::of(
            ErrorCode::Unsupported,
            "keys are pressed on a line, and a session that is written to is on none",
        )),
        FromClient::Hello { .. } | FromClient::Played { .. } | FromClient::PlayedRtp { .. } => {
            Err(Problem::of(
                ErrorCode::OutOfPlace,
                "this is said on a connection that carries audio",
            ))
        }
        FromClient::Unknown => Err(refusal(Refusal::Unknown)),
    }
}

async fn input(
    State(daemon): Daemon,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, Refused> {
    let id = session(&id)?;
    let who = who(&daemon, &headers, id, false).await?;
    let input = if who == Who::Control {
        asked_by_control(read(&daemon, &headers, body).await?)?
    } else {
        said_by_client(read(&daemon, &headers, body).await?)?
    };
    daemon.live(id, false).await?;
    match daemon.stack.send(id, input) {
        Ok(()) => Ok(StatusCode::ACCEPTED.into_response()),
        Err(ControlError::Behind) => Err(Problem::of(
            ErrorCode::Busy,
            "the session has too much waiting for it already",
        )
        .into()),
        Err(ControlError::NoSuchSession) => {
            Err(Problem::of(ErrorCode::NoSession, "the session is over").into())
        }
    }
}

async fn events(
    State(daemon): Daemon,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Result<Response, Refused> {
    let id = session(&id)?;
    let query = query.as_deref();
    // Where the client was: as a browser says it when it comes back by
    // itself, or as the address says it.
    let position = said(&headers, "last-event-id")
        .map(str::to_owned)
        .or_else(|| values(query, "after").next());
    let position = match position {
        None => None,
        Some(written) => Some(Position::read(&written).ok_or_else(|| {
            let detail = "where a client was is a life and a number, as the `id` of an event";
            Problem::of(ErrorCode::Malformed, detail)
        })?),
    };
    let who = who(&daemon, &headers, id, true).await?;
    let live = daemon.live(id, position.is_none()).await?;
    let resume_token = if who == Who::Connected {
        Some(daemon.credential(id).await?.to_string())
    } else {
        None
    };

    let wanted: Vec<String> = values(query, "features").collect();
    let tool_text = who == Who::Control && daemon.show_tool_text;
    let asked = Asked::of(wanted.iter().map(String::as_str), tool_text);
    let granted = asked.features();
    let next_seq = daemon
        .stack
        .status(id)
        .map_or(1, |status| status.next_seq.get());
    let welcome = FromServer::Welcome {
        v: client::VERSION,
        session: id.to_string(),
        life: live.life,
        resumed: position.is_some(),
        resume_token,
        audio: None,
        features: granted,
        next_seq,
        // A session that is written to is on no line, and ends for no
        // connection that drops.
        grace_ms: None,
    };
    let following = Following::begin(
        Arc::clone(&daemon),
        id,
        live.life,
        asked,
        &welcome,
        live.told,
        position,
    );
    let stream = stream::unfold(following, |mut following| async move {
        let event = following.next().await?;
        Some((Ok::<_, Infallible>(event), following))
    });
    let mut response = Sse::new(stream).into_response();
    // Whatever stands between the daemon and the client is to pass each
    // event on as it comes.
    let unbuffered = HeaderName::from_static("x-accel-buffering");
    response
        .headers_mut()
        .insert(unbuffered, HeaderValue::from_static("no"));
    Ok(response)
}
