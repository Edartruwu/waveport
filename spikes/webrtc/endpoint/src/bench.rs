//! str0m against itself in memory: what a handshake and a packet cost on the
//! answering side (ICE-lite, DTLS server), with the crypto provider of this
//! spike. No sockets, no browser: one thread hands each datagram across.
//!
//!   spike-endpoint bench [--handshakes 200] [--packets 200000]

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use str0m::config::DtlsVersion;
use str0m::format::Codec;
use str0m::media::{MediaKind, Mid};
use str0m::net::{Protocol, Receive};
use str0m::rtp::{RtpWrite, Ssrc};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig};

fn kind_of(b: &[u8]) -> &'static str {
    match b.first() {
        Some(0..=3) => "stun",
        Some(20..=63) => "dtls",
        Some(128..=191) => {
            if b.len() > 1 && (192..=223).contains(&b[1]) {
                "rtcp"
            } else {
                "rtp"
            }
        }
        _ => "other",
    }
}

#[derive(Default)]
struct Samples(Vec<u32>);
impl Samples {
    fn add(&mut self, d: Duration) {
        self.0.push(d.as_nanos().min(u128::from(u32::MAX)) as u32);
    }
    fn json(&self) -> Value {
        if self.0.is_empty() {
            return json!({"n": 0});
        }
        let mut v = self.0.clone();
        v.sort_unstable();
        let at = |p: f64| f64::from(v[((v.len() - 1) as f64 * p) as usize]) / 1000.0;
        let sum: u64 = v.iter().map(|x| u64::from(*x)).sum();
        json!({"n": v.len(), "p50_us": at(0.5), "p90_us": at(0.9), "p99_us": at(0.99), "p999_us": at(0.999), "max_us": at(1.0),
               "mean_us": sum as f64 / v.len() as f64 / 1000.0})
    }
}

struct Peer {
    rtc: Rtc,
    addr: SocketAddr,
    out: VecDeque<Vec<u8>>,
    connected: bool,
    rtp_in: u64,
    /// Time inside str0m for one datagram in: `handle_input` and the polls after it.
    cost: BTreeMap<&'static str, Samples>,
    cost_max_before_connected: Duration,
    cost_before_connected: Duration,
    datagrams_before_connected: (u32, u32),
    measure: bool,
}

impl Peer {
    fn new(rtc: Rtc, addr: &str) -> Self {
        Self {
            rtc,
            addr: addr.parse().expect("an address"),
            out: VecDeque::new(),
            connected: false,
            rtp_in: 0,
            cost: BTreeMap::new(),
            cost_max_before_connected: Duration::ZERO,
            cost_before_connected: Duration::ZERO,
            datagrams_before_connected: (0, 0),
            measure: false,
        }
    }

    /// Polls until str0m asks for time; returns the time spent inside it.
    fn drain(&mut self) -> Duration {
        let mut inside = Duration::ZERO;
        loop {
            let started = Instant::now();
            let out = self.rtc.poll_output();
            inside += started.elapsed();
            match out {
                Ok(Output::Timeout(_)) => break,
                Ok(Output::Transmit(t)) => {
                    if !self.connected {
                        self.datagrams_before_connected.1 += 1;
                    }
                    self.out.push_back(t.contents.to_vec());
                }
                Ok(Output::Event(Event::Connected)) => self.connected = true,
                Ok(Output::Event(Event::RtpPacket(_))) => self.rtp_in += 1,
                Ok(Output::Event(_)) => {}
                Err(e) => panic!("poll: {e}"),
            }
        }
        inside
    }

    fn take(&mut self, data: &[u8], from: SocketAddr, now: Instant) {
        let kind = kind_of(data);
        let was = self.connected;
        let receive = Receive::new(Protocol::Udp, from, self.addr, data).expect("a datagram");
        let started = Instant::now();
        self.rtc.handle_input(Input::Receive(now, receive)).expect("input");
        let took = started.elapsed() + self.drain();
        if self.measure {
            self.cost.entry(kind).or_default().add(took);
        }
        if !was {
            self.datagrams_before_connected.0 += 1;
            self.cost_before_connected += took;
            self.cost_max_before_connected = self.cost_max_before_connected.max(took);
        }
    }
}

fn config(dtls: DtlsVersion, lite: bool) -> RtcConfig {
    let mut cfg = RtcConfig::new()
        .set_crypto_provider(Arc::new(spike_provider::provider()))
        .set_rtp_mode(true)
        .set_ice_lite(lite)
        .set_dtls_version(dtls)
        .clear_codecs()
        .enable_opus(true, true);
    let _ = cfg.codec_config();
    cfg
}

/// Two peers, connected by hand as the tests of str0m do it, the answering one
/// ICE-lite and the DTLS server.
fn pair(dtls: DtlsVersion, start: Instant) -> (Peer, Peer) {
    let mut c = Peer::new(config(dtls, false).build(start), "10.0.0.1:5000");
    let mut s = Peer::new(config(dtls, true).build(start), "10.0.0.2:47000");
    s.measure = true;
    let cand_c = Candidate::host(c.addr, "udp").expect("candidate");
    let cand_s = Candidate::host(s.addr, "udp").expect("candidate");
    c.rtc.add_local_candidate(cand_c.clone());
    c.rtc.add_remote_candidate(cand_s.clone());
    s.rtc.add_local_candidate(cand_s);
    s.rtc.add_remote_candidate(cand_c);
    let finger_c = c.rtc.direct_api().local_dtls_fingerprint().clone();
    let finger_s = s.rtc.direct_api().local_dtls_fingerprint().clone();
    c.rtc.direct_api().set_remote_fingerprint(finger_s);
    s.rtc.direct_api().set_remote_fingerprint(finger_c);
    let creds_c = c.rtc.direct_api().local_ice_credentials();
    let creds_s = s.rtc.direct_api().local_ice_credentials();
    c.rtc.direct_api().set_remote_ice_credentials(creds_s);
    s.rtc.direct_api().set_remote_ice_credentials(creds_c);
    c.rtc.direct_api().set_ice_controlling(true);
    s.rtc.direct_api().set_ice_controlling(false);
    c.rtc.direct_api().start_dtls(true).expect("dtls");
    s.rtc.direct_api().start_dtls(false).expect("dtls");
    (c, s)
}

/// Hands every waiting datagram across, both ways, until nothing moves.
fn exchange(c: &mut Peer, s: &mut Peer, now: Instant) -> bool {
    let mut moved = false;
    loop {
        c.drain();
        s.drain();
        if c.out.is_empty() && s.out.is_empty() {
            return moved;
        }
        moved = true;
        while let Some(d) = c.out.pop_front() {
            s.take(&d, c.addr, now);
        }
        while let Some(d) = s.out.pop_front() {
            c.take(&d, s.addr, now);
        }
    }
}

fn connect(dtls: DtlsVersion) -> Option<(Peer, Peer, Instant)> {
    let mut now = Instant::now();
    let (mut c, mut s) = pair(dtls, now);
    for _ in 0..4000 {
        if c.connected && s.connected {
            return Some((c, s, now));
        }
        if !exchange(&mut c, &mut s, now) {
            now += Duration::from_millis(1);
            c.rtc.handle_input(Input::Timeout(now)).ok()?;
            s.rtc.handle_input(Input::Timeout(now)).ok()?;
        }
    }
    None
}

fn handshakes(dtls: DtlsVersion, n: usize) -> Value {
    let (mut total, mut worst, mut failed) = (Samples::default(), Samples::default(), 0u32);
    let mut cert = Samples::default();
    let mut datagrams = (0u32, 0u32);
    let mut version = None;
    for _ in 0..n {
        let started = Instant::now();
        let made = spike_provider::cert::generate();
        cert.add(started.elapsed());
        drop(made);
        match connect(dtls) {
            Some((_c, mut s, _)) => {
                total.add(s.cost_before_connected);
                worst.add(s.cost_max_before_connected);
                datagrams = s.datagrams_before_connected;
                version = s.rtc.direct_api().dtls_protocol_version().map(|v| format!("{v:?}"));
            }
            None => failed += 1,
        }
    }
    json!({
        "asked_for": format!("{dtls:?}"), "negotiated": version, "runs": n, "failed": failed,
        "answering_side_datagrams_in_out_until_connected": [datagrams.0, datagrams.1],
        "answering_side_cpu_until_connected": total.json(),
        "answering_side_costliest_single_datagram": worst.json(),
        "making_a_certificate": cert.json(),
    })
}

fn packets(n: usize, payload: usize) -> Value {
    let Some((mut c, mut s, mut now)) = connect(DtlsVersion::Dtls12) else {
        return json!({"error": "no connection"});
    };
    let mid: Mid = "aud".into();
    let (ssrc_c, ssrc_s): (Ssrc, Ssrc) = (1111.into(), 2222.into());
    for (p, tx, rx) in [(&mut c, ssrc_c, ssrc_s), (&mut s, ssrc_s, ssrc_c)] {
        let mut api = p.rtc.direct_api();
        api.declare_media(mid, MediaKind::Audio);
        api.declare_stream_tx(tx, None, mid, None);
        api.expect_stream_rx(rx, None, mid, None);
    }
    let pt = s.rtc.codec_config().params().iter().find(|p| p.spec().codec == Codec::Opus).map(|p| p.pt()).expect("opus");
    let body = vec![0x5au8; payload];
    let mut out_cost = Samples::default();
    s.cost.clear();
    let before = s.rtp_in;
    for i in 0..n as u64 {
        now += Duration::from_millis(20);
        // One packet from the far side in.
        if let Some(stream) = c.rtc.direct_api().stream_tx(&ssrc_c) {
            stream.write_rtp(RtpWrite::new(pt, (1000 + i).into(), (i * 960) as u32, now, body.clone()));
        }
        c.rtc.handle_input(Input::Timeout(now)).expect("time");
        c.drain();
        // One packet of ours out: the write, the timer, and the polls that encrypt it.
        let started = Instant::now();
        if let Some(stream) = s.rtc.direct_api().stream_tx(&ssrc_s) {
            stream.write_rtp(RtpWrite::new(pt, (5000 + i).into(), (i * 960) as u32, now, body.clone()));
        }
        s.rtc.handle_input(Input::Timeout(now)).expect("time");
        let took = started.elapsed() + s.drain();
        out_cost.add(took);
        exchange(&mut c, &mut s, now);
    }
    let cost: BTreeMap<&str, Value> = s.cost.iter().map(|(k, v)| (*k, v.json())).collect();
    json!({
        "payload_bytes": payload, "packets_each_way": n, "rtp_packets_received_by_the_answering_side": s.rtp_in - before,
        "one_datagram_in": cost, "one_packet_out": out_cost.json(),
    })
}

pub fn run(args: &[String]) {
    let arg = |name: &str, d: usize| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(d);
    let n_h = arg("--handshakes", 200);
    let n_p = arg("--packets", 200_000);
    // Not measured: the first handshakes of a process, which also pay for cold caches.
    let _ = handshakes(DtlsVersion::Dtls12, 30);
    let _ = handshakes(DtlsVersion::Dtls13, 30);
    let out = json!({
        "what": "str0m 0.25.0 against itself in memory, pure-Rust crypto provider of this spike, release build, one thread; times are inside str0m only",
        "handshake_dtls12": handshakes(DtlsVersion::Dtls12, n_h),
        "handshake_dtls13": handshakes(DtlsVersion::Dtls13, n_h),
        "handshake_auto": handshakes(DtlsVersion::Auto, n_h),
        "packets_80_bytes": packets(n_p, 80),
        "packets_170_bytes_as_with_red": packets(n_p, 170),
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
}
