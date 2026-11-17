//! The line of a session: a WebSocket its far end is on.
//!
//! `GET /v1/client` asks for the connection to become a WebSocket that
//! speaks client protocol v1. The request says nothing of who asks: a
//! browser cannot say it in a header, and what is said in an address is
//! written down by whatever stands between. Who asks is said in the
//! first message, the hello: with a client token, which was made for
//! one session and is used up by this, or with the resume token of a
//! client that comes back.
//!
//! A session whose far end comes this way is on a line. It hears and
//! speaks if the hello names audio and its agent does either, and reads
//! and writes otherwise. When the connection drops the session waits
//! for it, and when the daemon is started again the session is gone on
//! with on the connection its far end comes back on.

use core::time::Duration;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::{
    CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_PROTOCOL,
    SEC_WEBSOCKET_VERSION, UPGRADE,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use waveport_adapter_kit::ws::{self, WsConn};
use waveport_assembly::ResumeToken;
use waveport_assembly::shown::{self, Asked, Follower, Position};
use waveport_ports::control::SessionInput;
use waveport_ports::journal::SessionId;
use waveport_protocol::client::v1::{ByeReason, ErrorCode, FromClient, FromServer, Resume};
use waveport_protocol::control::v1::Problem;
use waveport_transport_websocket::{Caller, GREETING_TIMEOUT, Hello, Link, PROTOCOL};

use crate::daemon::{Shared, failed, raised};
use crate::http::{Refused, not_admitted, problem, said};

/// The version of the WebSocket protocol there is.
const WEBSOCKET: &str = "13";

/// How long a line has to close once the daemon has said on it that it
/// is going away. A daemon that stops waits a second for what holds its
/// sessions, and a client has 300 ms to answer the close of its line.
const LINE_CLOSES_WITHIN: Duration = Duration::from_millis(500);

/// Whether `headers` name `value` under `name`, among whatever else
/// they name there.
fn names(headers: &HeaderMap, name: HeaderName, value: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|named| named.to_str().ok())
        .flat_map(|named| named.split(','))
        .any(|named| named.trim().eq_ignore_ascii_case(value))
}

/// An answer that says what a connection has to change into.
fn upgrade_to(code: ErrorCode, detail: &str, name: HeaderName, value: &'static str) -> Response {
    let refused = problem(StatusCode::UPGRADE_REQUIRED, code, detail);
    let mut response = Refused::from(refused).into_response();
    let headers = response.headers_mut();
    headers.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
    headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(name, HeaderValue::from_static(value));
    response
}

/// Answers a request for a client's line: the connection becomes a
/// WebSocket, and whoever is on it is heard from there on.
pub(crate) async fn client(
    State(daemon): State<Arc<Shared>>,
    mut request: Request,
) -> Result<Response, Refused> {
    if daemon.is_draining() {
        return Err(Shared::busy().into());
    }
    if request.uri().query().is_some() {
        let detail = "the address of a client's line says nothing: who connects is said in its \
                      hello";
        return Err(Problem::of(ErrorCode::Malformed, detail).into());
    }
    let headers = request.headers();
    if !(names(headers, UPGRADE, "websocket") && names(headers, CONNECTION, "upgrade")) {
        let detail = "a client connects here over a WebSocket";
        return Ok(upgrade_to(
            ErrorCode::Unsupported,
            detail,
            UPGRADE,
            "websocket",
        ));
    }
    if said(headers, SEC_WEBSOCKET_VERSION) != Some(WEBSOCKET) {
        let detail = "the WebSocket protocol spoken here is version 13";
        let name = SEC_WEBSOCKET_VERSION;
        return Ok(upgrade_to(ErrorCode::Version, detail, name, WEBSOCKET));
    }
    let offered = headers.get_all(SEC_WEBSOCKET_PROTOCOL).iter();
    if !ws::offers(offered.filter_map(|named| named.to_str().ok()), PROTOCOL) {
        let detail = format!(
            "the protocol spoken here is {PROTOCOL}: a client names it among the ones it speaks"
        );
        return Err(Problem::of(ErrorCode::Version, detail).into());
    }
    let accept = said(headers, SEC_WEBSOCKET_KEY)
        .map(ws::accept_key)
        .and_then(|accept| HeaderValue::from_str(&accept).ok());
    let Some(accept) = accept else {
        let detail = "a request for a WebSocket has a key";
        return Err(Problem::of(ErrorCode::Malformed, detail).into());
    };
    let upgraded = hyper::upgrade::on(&mut request);
    tokio::spawn(serve(Arc::clone(&daemon), upgraded));
    let mut response = StatusCode::SWITCHING_PROTOCOLS.into_response();
    let headers = response.headers_mut();
    headers.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
    headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(SEC_WEBSOCKET_ACCEPT, accept);
    headers.insert(SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static(PROTOCOL));
    Ok(response)
}

/// The session a hello is for, and where its client was if it comes
/// back.
async fn admitted(daemon: &Shared, hello: &Hello) -> Result<(SessionId, Option<Resume>), Problem> {
    if let Some(resume) = &hello.resume {
        let none = || not_admitted("this is no resume token of a session");
        let token: ResumeToken = resume.token.parse().map_err(|_| none())?;
        let admitted = daemon
            .stack
            .kept
            .admits(&token)
            .await
            .map_err(|failure| failed("the token could not be looked at", &failure))?;
        return if admitted {
            Ok((token.session(), Some(resume.clone())))
        } else {
            Err(none())
        };
    }
    let Some(token) = &hello.token else {
        return Err(not_admitted(
            "a hello says which session it is for: with a client token, or with the token its \
             client came back with",
        ));
    };
    daemon
        .tickets
        .redeem_for_its_session(token)
        .map(|id| (id, None))
        .ok_or_else(|| {
            not_admitted("this is no client token: it was used, is too old, or was never one")
        })
}

/// One connection, from the answer that made it a WebSocket to its end.
async fn serve(daemon: Arc<Shared>, upgraded: OnUpgrade) {
    // A client that left before it was answered was never there.
    let Ok(upgraded) = upgraded.await else {
        return;
    };
    let conn = WsConn::from_upgraded(TokioIo::new(upgraded)).await;
    let mut going = daemon.going();
    let greeted = tokio::select! {
        greeted = Caller::of(conn, None, GREETING_TIMEOUT) => greeted,
        // Whoever has not said who they are is not waited for.
        () = raised(&mut going) => return,
    };
    // One that did not begin as a client does has been told so.
    let Ok(caller) = greeted else {
        return;
    };
    let (id, back) = match admitted(&daemon, caller.hello()).await {
        Ok(admitted) => admitted,
        Err(problem) => {
            let why = problem.detail.as_deref().unwrap_or_default();
            caller.refuse(problem.code, why).await;
            return;
        }
    };
    // What the tools were given and what they answered is for whoever
    // holds a control key, which no client does.
    let wanted = caller.hello().features.iter().map(String::as_str);
    let asked = Asked::of(wanted, false);
    let position = back.as_ref().map(|resume| Position {
        life: resume.life,
        seen: resume.seen,
    });
    let Some((link, life)) = daemon.on_line(id, caller, back.as_ref(), asked).await else {
        return;
    };
    tells(&daemon, id, life, asked, link, position).await;
}

/// Tells the client on one connection of the session `id`, which is in
/// the life `life`, what the session does, and does what the client
/// asks that its line cannot. `back` is where a client that came back
/// was.
///
/// It ends with the connection, with the session, whose goodbye follows
/// what was told here, or with the daemon, which says that it is going
/// away: the session is kept, and its client comes back to the daemon
/// that is started next.
async fn tells(
    daemon: &Shared,
    id: SessionId,
    life: u64,
    asked: Asked,
    mut link: Link,
    back: Option<Position>,
) {
    let stack = &daemon.stack;
    let mut going = daemon.going();
    let mut follower = Follower::begin(stack, id, life, asked, None, back);
    loop {
        tokio::select! {
            () = raised(&mut going) => {
                let reason = ByeReason::GoingAway;
                if link.tell(FromServer::Bye { reason }).await.is_ok() {
                    // The goodbye is said, and the line closed, on the
                    // session's own thread. The daemon stops its
                    // sessions once nobody here holds them: so the line
                    // is held until it is over, which is when nothing
                    // more is asked on it.
                    let over = async { while link.asked().await.is_some() {} };
                    let _ = tokio::time::timeout(LINE_CLOSES_WITHIN, over).await;
                }
                return;
            }
            next = follower.next(stack) => {
                let Some((message, _)) = next else {
                    return;
                };
                // How the session ended is the last thing there is to
                // tell.
                let last = shown::closes(&message);
                if link.tell(message).await.is_err() || last {
                    return;
                }
            }
            wanted = link.asked() => match wanted {
                Some(FromClient::Interrupt) => {
                    let _ = stack.ask(id, SessionInput::Interrupt);
                }
                Some(_) => {}
                None => return,
            },
        }
    }
}
