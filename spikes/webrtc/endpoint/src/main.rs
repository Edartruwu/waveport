#![recursion_limit = "512"]
//! Spike endpoint for decision 0005. `serve` runs the endpoint and its test
//! pages; `bench` measures str0m against itself in memory.

mod bench;
mod http;
mod media;
mod pipecat;
mod red;
mod signal;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpListener, UdpSocket};
use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use http::{Request, Response};
use media::{Cmd, Net, Opts};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

/// The address a browser on this machine can reach us at. Firefox does not
/// pair with 127.0.0.1 unless told to, so the first choice is the address of
/// the interface that has the default route.
fn local_ip() -> IpAddr {
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("192.0.2.1:9")?;
            s.local_addr()
        })
        .map(|a| a.ip())
        .unwrap_or_else(|_| IpAddr::from([127, 0, 0, 1]))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("bench") => bench::run(&args[2..]),
        _ => serve(&args),
    }
}

fn serve(args: &[String]) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let http_port: u16 = arg(args, "--http").and_then(|v| v.parse().ok()).unwrap_or(8787);
    let media_port: u16 = arg(args, "--media").and_then(|v| v.parse().ok()).unwrap_or(47_000);
    let ip: IpAddr = arg(args, "--ip").and_then(|v| v.parse().ok()).unwrap_or_else(local_ip);
    let results = arg(args, "--results").map_or_else(|| root.join("results"), PathBuf::from);
    std::fs::create_dir_all(&results).ok();

    // Bound to the address that is advertised, not to every address: a reply
    // from a socket bound to 0.0.0.0 leaves with whatever source address the
    // route to the peer gives, and Firefox drops an answer to its check that
    // comes from another address than the one it asked (Chrome did not mind).
    let bind: IpAddr = if arg(args, "--bind-any").is_some() { IpAddr::from([0, 0, 0, 0]) } else { ip };
    let udp = UdpSocket::bind((bind, media_port)).expect("the UDP port");
    let tcp = TcpListener::bind((bind, media_port)).expect("the TCP port");
    let speech_path = root.join("../../crates/testing/waveport-testkit/assets/what-time-is-it.wav");
    let speech = signal::load_speech(&speech_path.to_string_lossy());
    let net = Net { udp, udp_addr: SocketAddr::new(ip, media_port), tcp_addr: SocketAddr::new(ip, media_port), speech };
    eprintln!("media on {ip}:{media_port} (udp and tcp), pages on http://127.0.0.1:{http_port}/");

    let (tx, rx) = channel::<Cmd>();
    thread::spawn(move || media::run(net, rx));
    {
        let tx = tx.clone();
        thread::spawn(move || {
            for stream in tcp.incoming().flatten() {
                if let Ok(peer) = stream.peer_addr() {
                    let _ = tx.send(Cmd::Tcp(stream, peer));
                }
            }
        });
    }

    let tx = Arc::new(Mutex::new(tx));
    let web = root.join("web");
    let stub = Arc::new(pipecat::Stub::default());
    let listener = TcpListener::bind(("0.0.0.0", http_port)).expect("the HTTP port");
    http::serve(
        listener,
        Arc::new(move |req: Request| {
            let tx = tx.lock().map(|t| t.clone()).expect("the channel");
            route(&req, &tx, &web, &results, &speech_path, &stub)
        }),
    );
}

fn ask<T>(tx: &Sender<Cmd>, make: impl FnOnce(Sender<T>) -> Cmd) -> Option<T> {
    let (reply, got) = channel();
    tx.send(make(reply)).ok()?;
    got.recv_timeout(Duration::from_secs(5)).ok()
}

pub fn offer(tx: &Sender<Cmd>, sdp: String, opts: Opts, id: Option<u32>) -> Result<(u32, String), String> {
    ask(tx, |reply| Cmd::Offer { sdp, opts, id, reply }).unwrap_or_else(|| Err("no answer from the endpoint".into()))
}

fn route(req: &Request, tx: &Sender<Cmd>, web: &PathBuf, results: &PathBuf, speech: &PathBuf, stub: &pipecat::Stub) -> Response {
    let id = req.query.get("id").and_then(|v| v.parse::<u32>().ok());
    if req.method == "OPTIONS" {
        return Response::new(204, "text/plain", Vec::new());
    }
    if let Some(resp) = pipecat::route(req, tx, stub, results) {
        return resp;
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/offer") => {
            let sdp = String::from_utf8_lossy(&req.body).into_owned();
            let name = req.query.get("label").cloned().unwrap_or_default();
            match offer(tx, sdp.clone(), Opts::from_query(&req.query), id) {
                Ok((id, answer)) => {
                    if !name.is_empty() {
                        let stem: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)).collect();
                        std::fs::create_dir_all(results.join("sdp")).ok();
                        std::fs::write(results.join("sdp").join(format!("{stem}.offer.sdp")), &sdp).ok();
                        std::fs::write(results.join("sdp").join(format!("{stem}.answer.sdp")), &answer).ok();
                    }
                    Response::json(serde_json::json!({"id": id, "sdp": answer}).to_string())
                }
                Err(e) => Response::text(400, e),
            }
        }
        // The same, in the shape of WHIP (RFC 9725): SDP in, 201 with SDP and a Location out.
        ("POST", "/whip") => {
            let sdp = String::from_utf8_lossy(&req.body).into_owned();
            match offer(tx, sdp, Opts::from_query(&req.query), None) {
                Ok((id, answer)) => Response::new(201, "application/sdp", answer)
                    .header("Location", &format!("/whip/{id}"))
                    .header("ETag", &format!("\"{id}-1\""))
                    .header("Access-Control-Expose-Headers", "Location, ETag, Link"),
                Err(e) => Response::text(400, e),
            }
        }
        ("POST", "/control") => match id.and_then(|id| ask(tx, |reply| Cmd::Control { id, q: req.query.clone(), reply })) {
            Some(out) => Response::json(out),
            None => Response::text(404, "no such session"),
        },
        ("GET", "/stats") => match id.and_then(|id| ask(tx, |reply| Cmd::Stats { id, reply })) {
            Some(out) => Response::json(out),
            None => Response::text(404, "no such session"),
        },
        ("POST", "/report") => {
            let name = req.query.get("name").cloned().unwrap_or_else(|| "report".into());
            let stem: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)).collect();
            let mut body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null);
            if let Some(server) = id.and_then(|id| ask(tx, |reply| Cmd::Stats { id, reply })) {
                body["server"] = serde_json::from_str(&server).unwrap_or(serde_json::Value::Null);
            }
            // The run is over: stop the session, or it goes on encoding for a browser that has left.
            if let Some(id) = id.filter(|_| req.query.get("keep").is_none()) {
                let mut q = std::collections::HashMap::new();
                q.insert("cmd".to_string(), "close".to_string());
                let _: Option<String> = ask(tx, |reply| Cmd::Control { id, q, reply });
            }
            let path = results.join(format!("{stem}.json"));
            match std::fs::write(&path, serde_json::to_vec_pretty(&body).unwrap_or_default()) {
                Ok(()) => {
                    eprintln!("report: {}", path.display());
                    Response::json("{}")
                }
                Err(e) => Response::text(400, e.to_string()),
            }
        }
        ("GET", "/speech.wav") => match std::fs::read(speech) {
            Ok(b) => Response::new(200, "audio/wav", b),
            Err(_) => Response::text(404, "no speech"),
        },
        ("GET", path) => {
            let rel = if path == "/" { "index.html" } else { path.trim_start_matches('/') };
            if rel.contains("..") {
                return Response::text(400, "no");
            }
            match std::fs::read(web.join(rel)) {
                Ok(b) => {
                    let ctype = match rel.rsplit('.').next() {
                        Some("html") => "text/html; charset=utf-8",
                        Some("js" | "mjs") => "text/javascript; charset=utf-8",
                        Some("json") => "application/json",
                        Some("wav") => "audio/wav",
                        Some("map") => "application/json",
                        _ => "application/octet-stream",
                    };
                    Response::new(200, ctype, b)
                }
                Err(_) => Response::text(404, "not found"),
            }
        }
        _ => Response::text(404, "not found"),
    }
}

#[allow(dead_code)]
fn unused(_: HashMap<String, String>) {}
