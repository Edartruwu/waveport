//! A very small HTTP/1.1 server: one thread per connection, one request per
//! connection. Throwaway.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

pub struct Response {
    pub status: u16,
    pub ctype: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, ctype: &str, body: impl Into<Vec<u8>>) -> Self {
        Self { status, ctype: ctype.into(), headers: Vec::new(), body: body.into() }
    }
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self::new(status, "text/plain", body.into().into_bytes())
    }
    pub fn json(body: impl Into<String>) -> Self {
        Self::new(200, "application/json", body.into().into_bytes())
    }
    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
}

pub type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

pub fn serve(listener: TcpListener, handler: Handler) {
    for stream in listener.incoming().flatten() {
        let handler = handler.clone();
        thread::spawn(move || {
            let _ = one(stream, &handler);
        });
    }
}

fn pct(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let h = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                if let Some(v) = h {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn one(mut stream: TcpStream, handler: &Handler) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let head_end = loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        if buf.len() > 1 << 20 {
            return Ok(());
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let mut headers = HashMap::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    let (path, qs) = target.split_once('?').unwrap_or((&target, ""));
    let mut query = HashMap::new();
    for kv in qs.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        query.insert(pct(k), pct(v));
    }
    let resp = handler(Request { method, path: path.to_string(), query, headers, body });
    let reason = match resp.status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Status",
    };
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: *\r\nAccess-Control-Allow-Methods: GET, POST, PATCH, DELETE, OPTIONS\r\n",
        resp.status,
        reason,
        resp.ctype,
        resp.body.len()
    );
    for (k, v) in &resp.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    stream.write_all(out.as_bytes())?;
    stream.write_all(&resp.body)?;
    stream.flush()
}
