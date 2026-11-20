//! The page a daemon serves to try its agents on.
//!
//! `GET /playground/` is a page that makes a session of an agent and
//! talks or writes to it, with the JavaScript client (`sdks/js`) served
//! beside it. Both are part of the program: the files are read when it
//! is built, and nothing is read from a disk or fetched from anywhere
//! when it runs.
//!
//! The page asks as anyone asks, over the control API and a client's
//! line, so it can do what whoever opens it could do without it and no
//! more: where the daemon has a control key, the page asks its user for
//! it. It is served by a daemon that listens on this machine alone,
//! unless the configuration says otherwise (`playground`), and behind
//! the same look at where a request goes and where it comes from as
//! everything else.
//!
//! A page from here runs nothing that is not from here, and is not
//! shown inside another page.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, LOCATION, REFERRER_POLICY,
    X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use waveport_protocol::client::v1::ErrorCode;

use crate::daemon::Shared;
use crate::http::{Refused, problem};

const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";
const JSON: &str = "application/json";
const SVG: &str = "image/svg+xml; charset=utf-8";

/// Brings in a file of the JavaScript client's folder.
macro_rules! file {
    ($path:literal) => {
        include_str!(concat!("../../../sdks/js/", $path))
    };
}

/// What is served under `/playground/`: the name, what kind of thing it
/// is, and the thing.
const FILES: [(&str, &str, &str); 10] = [
    ("", HTML, file!("playground/index.html")),
    ("playground.css", CSS, file!("playground/playground.css")),
    // A browser that is given no icon asks for one where there is
    // none, and says that the page's policy refused it.
    ("icon.svg", SVG, file!("playground/icon.svg")),
    ("playground.js", JS, file!("playground/playground.js")),
    ("sdk/index.js", JS, file!("src/index.js")),
    ("sdk/client.js", JS, file!("src/client.js")),
    ("sdk/protocol.js", JS, file!("src/protocol.js")),
    ("sdk/audio.js", JS, file!("src/audio.js")),
    ("sdk/webrtc.js", JS, file!("src/webrtc.js")),
    ("sdk/worklet.js", JS, file!("src/worklet.js")),
];

/// What a page from here may do: run and show what is from here, talk
/// to where it is from, and nothing else. It is shown in no other page.
const POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
                      img-src 'self'; connect-src 'self'; worker-src 'self'; \
                      base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Something of the page, as it is answered.
fn served(kind: &'static str, body: String) -> Response {
    let mut response = body.into_response();
    let headers = response.headers_mut();
    let mut says = |name: HeaderName, value: &'static str| {
        headers.insert(name, HeaderValue::from_static(value));
    };
    says(CONTENT_TYPE, kind);
    says(CONTENT_SECURITY_POLICY, POLICY);
    says(X_CONTENT_TYPE_OPTIONS, "nosniff");
    says(X_FRAME_OPTIONS, "DENY");
    says(REFERRER_POLICY, "no-referrer");
    // The page is the program's: another program may serve another.
    says(CACHE_CONTROL, "no-cache");
    says(
        HeaderName::from_static("permissions-policy"),
        "microphone=(self), camera=(), geolocation=()",
    );
    response
}

fn nothing_here() -> Response {
    let detail = "the playground has no such file";
    Refused::from(problem(StatusCode::NOT_FOUND, ErrorCode::NoSession, detail)).into_response()
}

/// Sends whoever asks for the page without the stroke that ends its
/// address to the address that has it: what the page names is beside
/// it.
async fn to_the_page() -> Response {
    let mut response = StatusCode::PERMANENT_REDIRECT.into_response();
    let there = HeaderValue::from_static("/playground/");
    response.headers_mut().insert(LOCATION, there);
    response
}

async fn page() -> Response {
    file(Path(String::new())).await
}

async fn file(Path(name): Path<String>) -> Response {
    FILES
        .iter()
        .find(|(known, _, _)| *known == name)
        .map_or_else(nothing_here, |(_, kind, body)| {
            served(kind, (*body).to_owned())
        })
}

/// What the page is told of the daemon: the agents there are to try,
/// and whether a control key is asked for.
async fn config(State(daemon): State<Arc<Shared>>) -> Response {
    let told = serde_json::json!({
        "agents": daemon.agent_names(),
        "key": daemon.wants_a_key(),
    });
    served(JSON, told.to_string())
}

/// The routes of the page.
pub(crate) fn routes() -> Router<Arc<Shared>> {
    Router::new()
        .route("/playground", get(to_the_page))
        .route("/playground/", get(page))
        .route("/playground/config.json", get(config))
        .route("/playground/{*name}", get(file))
}

#[cfg(test)]
mod tests {
    use super::{FILES, POLICY};

    #[test]
    fn what_the_page_names_is_served_beside_it() {
        let named = |name: &str| FILES.iter().any(|(known, _, _)| *known == name);
        let (_, _, page) = FILES[0];
        // What the page itself brings in.
        for brought in ["playground.css", "playground.js", "icon.svg"] {
            assert!(page.contains(&format!("\"{brought}\"")), "{brought}");
            assert!(named(brought), "{brought}");
        }
        // What each file of the client brings in, by the name it says.
        for (name, _, body) in FILES.iter().filter(|(name, _, _)| name.starts_with("sdk/")) {
            for line in body.lines() {
                let from = line
                    .split_once(" from \"./")
                    .or_else(|| line.split_once("import(\"./"))
                    .or_else(|| line.split_once("new URL(\"./"));
                if let Some((_, rest)) = from {
                    let brought = rest.split('"').next().unwrap_or_default();
                    assert!(named(&format!("sdk/{brought}")), "{name} names {brought}");
                }
            }
        }
        assert!(FILES.iter().all(|(_, _, body)| !body.is_empty()));
    }

    #[test]
    fn the_page_asks_for_nothing_its_policy_refuses() {
        let (_, _, page) = FILES[0];
        // Nothing is written into the page that a policy of `'self'`
        // would not run or show, and nothing is from anywhere else.
        for refused in [
            "<style",
            "style=",
            "onclick=",
            "javascript:",
            "http://",
            "https://",
        ] {
            assert!(!page.contains(refused), "{refused}");
        }
        assert_eq!(page.matches("<script").count(), 1);
        assert!(page.contains("<script type=\"module\" src=\"playground.js\">"));
        assert!(POLICY.contains("script-src 'self'") && !POLICY.contains("unsafe"));
    }
}
