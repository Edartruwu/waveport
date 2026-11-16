//! A stub of Pipecat's SmallWebRTC signalling (as read from `@pipecat-ai/client-js` 1.16.0
//! and `@pipecat-ai/small-webrtc-transport` 1.11.2) and of what its client
//! expects on the data channel. It answers with this spike's own endpoint:
//! ICE-lite, audio only, one socket.
//!
//!   POST  /start                         {sessionId, iceConfig}
//!   POST  /api/offer                     {sdp, type, pc_id, restart_pc} -> {sdp, type, pc_id}
//!   POST  /sessions/{id}/api/offer       the same
//!   PATCH (either)                       {pc_id, candidates: [...]}     -> {"status": "success"}
//!
//! Every request and reply is kept and written to `results/pipecat-http.json`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::mpsc::Sender;

use serde_json::{Value, json};

use crate::http::{Request, Response};
use crate::media::{Cmd, Opts};

#[derive(Default)]
pub struct Stub {
    /// pc_id -> the endpoint's session.
    peers: Mutex<HashMap<String, u32>>,
    log: Mutex<Vec<Value>>,
    next: Mutex<u32>,
}

fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
}

impl Stub {
    fn keep(&self, results: &PathBuf, entry: Value) {
        if let Ok(mut log) = self.log.lock() {
            log.push(entry);
            let _ = std::fs::write(results.join("pipecat-http.json"), serde_json::to_vec_pretty(&*log).unwrap_or_default());
        }
    }
}

pub fn route(req: &Request, tx: &Sender<Cmd>, stub: &Stub, results: &PathBuf) -> Option<Response> {
    let is_offer = req.path == "/api/offer" || (req.path.starts_with("/sessions/") && req.path.ends_with("/api/offer"));
    if req.path == "/pipecat-reset" {
        if let Ok(mut log) = stub.log.lock() {
            log.clear();
        }
        return Some(Response::json("{}"));
    }
    if !(req.path == "/start" || is_offer) {
        return None;
    }
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    let reply = match (req.method.as_str(), is_offer) {
        ("POST", false) => {
            let n = stub.next.lock().map(|mut n| {
                *n += 1;
                *n
            });
            json!({"sessionId": format!("spike-session-{}", n.unwrap_or(0)), "iceConfig": {"iceServers": []}})
        }
        ("POST", true) => {
            let sdp = body["sdp"].as_str().unwrap_or_default().to_string();
            let pc_id = body["pc_id"].as_str().map(str::to_string);
            let restart = body["restart_pc"].as_bool().unwrap_or(false);
            let known = pc_id.as_ref().and_then(|p| stub.peers.lock().ok().and_then(|m| m.get(p).copied()));
            let mut q = req.query.clone();
            q.insert("pipecat".into(), "1".into());
            q.entry("label".into()).or_insert_with(|| "pipecat".into());
            // A known pc_id renegotiates on the same connection, unless the
            // client asks for a new one.
            let id = if restart { None } else { known };
            match crate::offer(tx, sdp, Opts::from_query(&q), id) {
                Ok((session, answer)) => {
                    let pc_id = match (id, pc_id) {
                        (Some(_), Some(p)) => p,
                        _ => format!("SmallWebRTCConnection#{session}"),
                    };
                    if let Ok(mut m) = stub.peers.lock() {
                        m.insert(pc_id.clone(), session);
                    }
                    json!({"sdp": answer, "type": "answer", "pc_id": pc_id})
                }
                Err(e) => {
                    stub.keep(results, json!({"at_ms": now_ms(), "method": req.method, "path": req.path, "request": body, "status": 400, "reply": e}));
                    return Some(Response::text(400, e));
                }
            }
        }
        ("PATCH", true) => {
            let known = body["pc_id"].as_str().and_then(|p| stub.peers.lock().ok().map(|m| m.contains_key(p))).unwrap_or(false);
            if !known {
                stub.keep(results, json!({"at_ms": now_ms(), "method": req.method, "path": req.path, "request": body, "status": 404}));
                return Some(Response::text(404, "Peer connection not found"));
            }
            // ICE-lite: the client's candidates are not needed; its checks say where it is.
            json!({"status": "success"})
        }
        _ => return None,
    };
    stub.keep(results, json!({"at_ms": now_ms(), "method": req.method, "path": req.path, "query": req.query, "request": body, "status": 200, "reply": reply}));
    Some(Response::json(reply.to_string()))
}

/// What the stub says on the data channel. `client-ready` is answered with
/// `bot-ready` under the same id; then a few events a bot would send, to see
/// which of them the stock client hands to its callbacks.
pub fn on_message(_session: u32, text: &str, mut reply: impl FnMut(String)) {
    if text.starts_with("ping") {
        return;
    }
    let Ok(msg) = serde_json::from_str::<Value>(text) else { return };
    if msg["type"] == "signalling" {
        return;
    }
    if msg["label"] != "rtvi-ai" {
        return;
    }
    let rtvi = |kind: &str, id: Value, data: Value| json!({"label": "rtvi-ai", "type": kind, "id": id, "data": data}).to_string();
    match msg["type"].as_str() {
        Some("client-ready") => {
            reply(rtvi("bot-ready", msg["id"].clone(), json!({"version": "2.3.0", "about": {"library": "webrtc-spike", "library_version": "0.0.0"}})));
            reply(rtvi("bot-started-speaking", Value::Null, Value::Null));
            reply(rtvi("bot-output", Value::Null, json!({"text": "Hello from the stub.", "spoken": true, "aggregated_by": "sentence"})));
            reply(rtvi("user-started-speaking", Value::Null, Value::Null));
            reply(rtvi("user-transcription", Value::Null, json!({"text": "what time is it", "user_id": "u", "timestamp": "2026-11-16T00:00:00Z", "final": true})));
            reply(rtvi("server-message", Value::Null, json!({"hello": "from the stub"})));
        }
        Some("disconnect-bot") => {}
        Some(_) => reply(rtvi("error-response", msg["id"].clone(), json!({"error": "not supported by the stub"}))),
        None => {}
    }
}
