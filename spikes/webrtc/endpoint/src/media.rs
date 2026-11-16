//! The endpoint: every peer on one UDP socket (and one TCP listener for ICE
//! over TCP), ICE-lite, RTP mode. One thread owns everything; the HTTP side
//! talks to it through a channel.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use str0m::change::SdpOffer;
use str0m::channel::{ChannelConfig, ChannelId};
use str0m::config::DtlsVersion;
use str0m::format::{Codec, CodecConfig, FormatParams};
use str0m::media::{Frequency, MediaKind, Mid, Pt};
use str0m::net::{Protocol, Receive, TcpType};
use str0m::rtp::RtpWrite;
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};

use crate::red;
use crate::signal;

/// What a session is asked to do, from the query of `POST /offer`.
#[derive(Debug, Clone)]
pub struct Opts {
    pub red: bool,
    pub red_dist: Vec<u32>,
    pub nack: bool,
    pub dtls: u8,
    pub loss_in: f64,
    pub loss_out: f64,
    pub seed: u64,
    pub fec: i32,
    pub udp: bool,
    pub tcp: bool,
    pub drop_udp: bool,
    pub src: String,
    pub frame_mode: bool,
    pub dcid: Option<u16>,
    pub tele: bool,
    pub tele_rates: String,
    pub send: bool,
    pub snap: bool,
    pub delay_ms: u64,
    pub label: String,
    pub pipecat: bool,
}

impl Opts {
    pub fn from_query(q: &HashMap<String, String>) -> Self {
        let flag = |k: &str, d: bool| q.get(k).map_or(d, |v| v == "1" || v == "true");
        let num = |k: &str, d: f64| q.get(k).and_then(|v| v.parse().ok()).unwrap_or(d);
        Self {
            red: flag("red", false),
            red_dist: q
                .get("red_dist")
                .map(|v| v.split(',').filter_map(|d| d.parse().ok()).collect())
                .unwrap_or_else(|| vec![1]),
            nack: flag("nack", false),
            dtls: num("dtls", 12.0) as u8,
            loss_in: num("loss_in", 0.0),
            loss_out: num("loss_out", 0.0),
            seed: num("seed", 1.0) as u64,
            fec: num("fec", 0.0) as i32,
            udp: flag("udp", true),
            tcp: flag("tcp", false),
            drop_udp: flag("drop_udp", false),
            src: q.get("src").cloned().unwrap_or_else(|| "tones".into()),
            frame_mode: flag("frame_mode", false),
            dcid: q.get("dcid").and_then(|v| v.parse().ok()),
            tele: q.get("tele").is_some_and(|v| v != "0"),
            tele_rates: q.get("tele").cloned().unwrap_or_default(),
            send: flag("send", true),
            snap: flag("snap", false),
            delay_ms: num("delay", 0.0) as u64,
            label: q.get("label").cloned().unwrap_or_default(),
            pipecat: flag("pipecat", false),
        }
    }
}

pub enum Cmd {
    Offer { sdp: String, opts: Opts, id: Option<u32>, reply: Sender<Result<(u32, String), String>> },
    Control { id: u32, q: HashMap<String, String>, reply: Sender<String> },
    Stats { id: u32, reply: Sender<String> },
    Tcp(TcpStream, SocketAddr),
}

pub struct Net {
    pub udp: UdpSocket,
    pub udp_addr: SocketAddr,
    pub tcp_addr: SocketAddr,
    pub speech: Vec<i16>,
}

/// xorshift64*, so that a run with a seed can be run again.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn wall_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
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
        json!({"n": v.len(), "p50_us": at(0.5), "p90_us": at(0.9), "p99_us": at(0.99), "max_us": at(1.0),
               "mean_us": sum as f64 / v.len() as f64 / 1000.0, "total_us": sum as f64 / 1000.0})
    }
}

/// What came in, kept by RTP time so that redundant copies can be placed.
#[derive(Default)]
struct Rx {
    packets: u64,
    bytes: u64,
    first_seq: Option<u64>,
    max_seq: u64,
    seqs: BTreeMap<u64, f64>,
    /// Sequence numbers we threw away on purpose before str0m saw them, with when
    /// and the RTP time in their header (which is in the clear).
    dropped: BTreeMap<u16, (f64, u32)>,
    /// RTP time (low 32 bits) of a frame whose own packet came after all, and how long after it was dropped.
    resent_frames: BTreeMap<u32, f64>,
    /// Frames that the LBRR inside a redundant block could give: the frame before that block's.
    via_red_lbrr: BTreeSet<u64>,
    /// A dropped one that arrived later all the same (a retransmission): how much later, ms.
    resent: BTreeMap<u16, f64>,
    dropped_again: u64,
    /// Frames (RTP time) whose own packet arrived, with when and whether it carries LBRR.
    direct: BTreeMap<u64, (f64, bool, bool)>,
    /// Frames first seen as a redundant block of a later packet.
    via_red: BTreeMap<u64, f64>,
    red_packets: u64,
    red_blocks: BTreeMap<String, u64>,
    plain_packets: u64,
    pts: BTreeMap<u8, u64>,
    modes: BTreeMap<String, u64>,
    sizes: Samples,
    tele: Vec<Value>,
    tele_all: Vec<Value>,
    odd: Vec<Value>,
    padding: u64,
    tele_last: Option<(u8, bool)>,
    symbols_ok: u64,
    symbols_bad: u64,
    blocks_silent: u64,
    last_symbol: Option<u32>,
    pcm: Vec<i16>,
    frame_delivery_ms: Vec<f64>,
    frames: u64,
    frames_noncontiguous: u64,
}

struct Sess {
    id: u32,
    rtc: Rtc,
    opts: Opts,
    created: Instant,
    created_wall: f64,
    mid: Option<Mid>,
    pt_opus: Option<Pt>,
    pt_red: Option<Pt>,
    pt_tele: Option<Pt>,
    tele_rate: u32,
    audio_t0: Option<Instant>,
    tx_seq: u64,
    tx_tick: u64,
    tx_ts0: u32,
    sending: bool,
    enc: opus::Encoder,
    dec: opus::Decoder,
    tx_hist: VecDeque<(u32, Vec<u8>)>,
    tx_log: Vec<(u64, u32, f64, bool)>,
    tx_dropped: BTreeSet<u16>,
    tx_bytes: u64,
    tx_tele: VecDeque<(u8, u32, u32, bool)>,
    tx_tele_gap: u32,
    tx_tele_ts48: bool,
    rx: Rx,
    rng_in: Rng,
    rng_out: Rng,
    chans: HashMap<ChannelId, String>,
    events: Vec<Value>,
    t_in: BTreeMap<&'static str, Samples>,
    t_drain: BTreeMap<&'static str, Samples>,
    t_whole: BTreeMap<&'static str, Samples>,
    t_send: Samples,
    t_idle: Samples,
    t_write: Samples,
    t_handshake: Duration,
    connected_at: Option<f64>,
    peer_tcp: Option<SocketAddr>,
    remote: Option<SocketAddr>,
    proto_seen: BTreeMap<String, u64>,
    dtls_version: Option<String>,
    dead: bool,
    stop_log: Vec<Value>,
    ping_count: u64,
    trace: Vec<Value>,
}

struct TcpConn {
    stream: TcpStream,
    peer: SocketAddr,
    buf: Vec<u8>,
    dead: bool,
}

struct Held {
    at: Instant,
    proto: Protocol,
    peer: SocketAddr,
    data: Vec<u8>,
}

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

fn build_rtc(net: &Net, opts: &Opts) -> Rtc {
    let mut cfg = RtcConfig::new()
        .set_crypto_provider(Arc::new(spike_provider::provider()))
        .set_ice_lite(true)
        .set_rtp_mode(!opts.frame_mode)
        .set_stats_interval(Some(Duration::from_secs(1)))
        .set_snap_enabled(opts.snap)
        .set_dtls_version(match opts.dtls {
            13 => DtlsVersion::Dtls13,
            0 => DtlsVersion::Auto,
            _ => DtlsVersion::Dtls12,
        });

    // Audio only: Opus, with RED folded onto it when asked, and telephone-event.
    let mut codecs = CodecConfig::empty();
    codecs.enable_opus(true, opts.red);
    if opts.tele {
        let mut rates = [(126u8, Frequency::EIGHT_KHZ), (110u8, Frequency::FORTY_EIGHT_KHZ)];
        if opts.tele_rates == "48first" {
            rates.reverse();
        }
        for (pt, rate) in rates {
            if (opts.tele_rates == "48" && pt == 126) || (opts.tele_rates == "8" && pt == 110) {
                continue;
            }
            codecs.add_config(pt.into(), None, Codec::Tele, rate, None, FormatParams::parse_line("0-16"));
        }
    }
    if opts.nack {
        let mut params = codecs.params().to_vec();
        for p in &mut params {
            if p.spec().codec == Codec::Opus {
                p.set_fb_nack(true);
            }
        }
        codecs = CodecConfig::new_from_payload_params(params);
    }
    codecs.set_red_distances(&opts.red_dist);
    *cfg.codec_config() = codecs;

    let mut rtc = cfg.build(Instant::now());
    if opts.udp {
        if let Ok(c) = Candidate::host(net.udp_addr, "udp") {
            rtc.add_local_candidate(c);
        }
    }
    if opts.tcp {
        if let Ok(c) = Candidate::builder().tcp().host(net.tcp_addr).tcptype(TcpType::Passive).build() {
            rtc.add_local_candidate(c);
        }
    }
    rtc
}

impl Sess {
    fn new(id: u32, net: &Net, opts: Opts) -> Result<Self, String> {
        let app = if opts.src == "speech" { opus::Application::Voip } else { opus::Application::Audio };
        let mut enc = opus::Encoder::new(48_000, opus::Channels::Mono, app).map_err(|e| e.to_string())?;
        enc.set_bitrate(opus::Bitrate::Bits(32_000)).map_err(|e| e.to_string())?;
        if opts.fec > 0 {
            enc.set_inband_fec(true).map_err(|e| e.to_string())?;
            enc.set_packet_loss_perc(opts.fec).map_err(|e| e.to_string())?;
        }
        let dec = opus::Decoder::new(48_000, opus::Channels::Mono).map_err(|e| e.to_string())?;
        let rtc = build_rtc(net, &opts);
        Ok(Self {
            id,
            rtc,
            rng_in: Rng(opts.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1),
            rng_out: Rng(opts.seed.wrapping_mul(0xD1B5_4A32_D192_ED03) | 1),
            sending: opts.send,
            opts,
            created: Instant::now(),
            created_wall: wall_ms(),
            mid: None,
            pt_opus: None,
            pt_red: None,
            pt_tele: None,
            tele_rate: 8000,
            audio_t0: None,
            tx_seq: 1000,
            tx_tick: 0,
            tx_ts0: 100_000,
            enc,
            dec,
            tx_hist: VecDeque::new(),
            tx_log: Vec::new(),
            tx_dropped: BTreeSet::new(),
            tx_bytes: 0,
            tx_tele: VecDeque::new(),
            tx_tele_gap: 0,
            tx_tele_ts48: false,
            rx: Rx::default(),
            chans: HashMap::new(),
            events: Vec::new(),
            t_in: BTreeMap::new(),
            t_drain: BTreeMap::new(),
            t_whole: BTreeMap::new(),
            t_send: Samples::default(),
            t_idle: Samples::default(),
            t_write: Samples::default(),
            t_handshake: Duration::ZERO,
            connected_at: None,
            peer_tcp: None,
            remote: None,
            proto_seen: BTreeMap::new(),
            dtls_version: None,
            dead: false,
            stop_log: Vec::new(),
            ping_count: 0,
            trace: Vec::new(),
        })
    }

    fn note(&mut self, what: &str, extra: Value) {
        self.events.push(json!({"t_ms": self.created.elapsed().as_secs_f64() * 1000.0, "wall_ms": wall_ms(), "what": what, "x": extra}));
    }

    fn accept(&mut self, sdp: &str) -> Result<String, String> {
        let offer = SdpOffer::from_sdp_string(sdp).map_err(|e| format!("offer: {e}"))?;
        if let Some(id) = self.opts.dcid {
            if self.chans.is_empty() {
                let cid = self.rtc.direct_api().create_data_channel(ChannelConfig {
                    label: "events".into(),
                    negotiated: Some(id),
                    ..Default::default()
                });
                self.chans.insert(cid, "events(pre-agreed)".into());
            }
        }
        let answer = self.rtc.sdp_api().accept_offer(offer).map_err(|e| format!("accept: {e}"))?;
        let answer = answer.to_sdp_string();
        self.note("answered", json!({}));
        Ok(answer)
    }

    /// Reads what was negotiated for the audio line, once it is there.
    fn learn_media(&mut self, mid: Mid) {
        self.mid = Some(mid);
        let Some(media) = self.rtc.media(mid) else { return };
        let mut seen = Vec::new();
        for p in media.remote_pts() {
            let Some(params) = self.rtc.codec_config().params().iter().find(|c| c.pt() == *p) else { continue };
            let spec = params.spec();
            seen.push(format!("{}={:?}/{} red={:?} nack={}", **p, spec.codec, spec.clock_rate.get(), params.red().map(|r| *r), params.fb_nack()));
            if spec.codec == Codec::Opus {
                self.pt_opus = Some(params.pt());
                self.pt_red = params.red();
            }
            if spec.codec == Codec::Tele && (self.pt_tele.is_none() || spec.clock_rate.get() == 48_000) {
                self.pt_tele = Some(params.pt());
                self.tele_rate = spec.clock_rate.get();
            }
        }
        self.note("media", json!({"mid": mid.to_string(), "negotiated": seen}));
        if self.opts.nack && !self.opts.frame_mode {
            // str0m asks for retransmissions only where there is an RTX stream,
            // and browsers offer none for audio; this turns the asking on.
            if let Some(rx) = self.rtc.direct_api().stream_rx_by_mid(mid, None) {
                rx.suppress_nack(false);
            }
        }
    }

    fn on_event(&mut self, ev: Event, net: &Net) {
        match ev {
            Event::Connected => {
                self.connected_at = Some(self.created.elapsed().as_secs_f64() * 1000.0);
                self.dtls_version = self.rtc.direct_api().dtls_protocol_version().map(|v| format!("{v:?}"));
                self.note("connected", json!({"dtls": self.dtls_version, "handshake_cpu_us": self.t_handshake.as_secs_f64() * 1e6}));
            }
            Event::IceConnectionStateChange(s) => {
                self.note("ice", json!(format!("{s:?}")));
                if s == IceConnectionState::Disconnected {
                    self.dead = true;
                }
            }
            Event::MediaAdded(m) => {
                if m.kind == MediaKind::Audio && self.mid.is_none() {
                    self.learn_media(m.mid);
                    self.note("media-added", json!({"mid": m.mid.to_string(), "dir": format!("{:?}", m.direction)}));
                }
            }
            Event::ChannelOpen(id, label) => {
                self.chans.entry(id).or_insert_with(|| label.clone());
                self.note("channel-open", json!({"label": label}));
                if !self.opts.pipecat {
                    let hello = json!({"type": "hello", "server_wall_ms": wall_ms()}).to_string();
                    if let Some(mut ch) = self.rtc.channel(id) {
                        let _ = ch.write(false, hello.as_bytes());
                    }
                }
            }
            Event::ChannelData(d) => {
                let text = String::from_utf8_lossy(&d.data).into_owned();
                if self.opts.pipecat {
                    crate::pipecat::on_message(self.id, &text, |reply| {
                        if let Some(mut ch) = self.rtc.channel(d.id) {
                            let _ = ch.write(false, reply.as_bytes());
                        }
                    });
                    if text.starts_with("ping") {
                        self.ping_count += 1;
                        if self.ping_count > 2 {
                            return;
                        }
                    }
                    self.note("channel-data", json!(text));
                    return;
                }
                if self.events.iter().filter(|e| e["what"] == "channel-data").count() < 20 {
                    self.note("channel-data", json!(text));
                }
                let echo = json!({"type": "echo", "server_wall_ms": wall_ms(), "of": text}).to_string();
                if let Some(mut ch) = self.rtc.channel(d.id) {
                    let _ = ch.write(false, echo.as_bytes());
                }
            }
            Event::ChannelClose(id) => {
                self.chans.remove(&id);
                self.note("channel-close", json!({}));
            }
            Event::RtpPacket(p) => self.on_rtp(&p),
            Event::MediaData(m) => {
                // Frame mode: only how late str0m hands a frame over.
                self.rx.frames += 1;
                if !m.contiguous {
                    self.rx.frames_noncontiguous += 1;
                }
                self.rx.frame_delivery_ms.push(m.network_time.elapsed().as_secs_f64() * 1000.0);
            }
            Event::PeerStats(_) | Event::MediaIngressStats(_) | Event::MediaEgressStats(_) => {}
            Event::Closed => self.dead = true,
            other => {
                if self.events.len() < 400 {
                    self.note("event", json!(format!("{other:?}").chars().take(160).collect::<String>()));
                }
            }
        }
        let _ = net;
    }

    fn on_rtp(&mut self, p: &str0m::rtp::RtpPacket) {
        let now = wall_ms();
        let pt = *p.header.payload_type;
        let seq = *p.seq_no;
        let time = p.time.numer();
        *self.rx.pts.entry(pt).or_default() += 1;
        if Some(p.header.payload_type) == self.pt_tele || (self.opts.tele && p.payload.len() == 4 && Some(p.header.payload_type) != self.pt_opus && Some(p.header.payload_type) != self.pt_red) {
            if let [event, flags, d0, d1] = *p.payload {
                let end = flags & 0x80 != 0;
                if self.rx.tele_all.len() < 400 {
                    self.rx.tele_all.push(json!([seq, time, event, end, u16::from_be_bytes([d0, d1]), pt, p.header.marker, (now * 10.0).round() / 10.0]));
                }
                if self.rx.tele_last != Some((event, end)) {
                    self.rx.tele_last = Some((event, end));
                    self.rx.tele.push(json!({"event": event, "end": end, "volume": flags & 0x3f, "duration": u16::from_be_bytes([d0, d1]), "pt": pt, "seq": seq, "rtp_time": time, "marker": p.header.marker, "wall_ms": now}));
                }
            }
            return;
        }
        if p.payload.is_empty() {
            // Padding only: Chrome probes for bandwidth on the audio stream when
            // transport-cc is negotiated. It takes a sequence number and carries no audio.
            self.rx.padding += 1;
            self.rx.seqs.insert(seq, now);
            return;
        }
        self.rx.packets += 1;
        self.rx.bytes += p.payload.len() as u64;
        self.rx.sizes.add(Duration::from_nanos(p.payload.len() as u64 * 1000));
        if self.rx.first_seq.is_none() {
            self.rx.first_seq = Some(seq);
        }
        self.rx.max_seq = self.rx.max_seq.max(seq);
        self.rx.seqs.insert(seq, now);
        let seq16 = seq as u16;
        if let Some((at, ts)) = self.rx.dropped.get(&seq16) {
            self.rx.resent.entry(seq16).or_insert(now - at);
            self.rx.resent_frames.entry(*ts).or_insert(now - at);
        }

        let primary: Vec<u8>;
        if Some(p.header.payload_type) == self.pt_red {
            self.rx.red_packets += 1;
            let Some(blocks) = red::unpack(&p.payload) else {
                *self.rx.red_blocks.entry("malformed".into()).or_default() += 1;
                if self.rx.odd.len() < 40 {
                    let hex: String = p.payload.iter().take(24).map(|b| format!("{b:02x}")).collect();
                    self.rx.odd.push(json!({"seq": seq, "rtp_time": time, "len": p.payload.len(), "marker": p.header.marker, "first_bytes": hex, "wall_ms": now}));
                }
                return;
            };
            let shape: Vec<String> = blocks.iter().map(|b| format!("{}@-{}", b.pt, b.offset)).collect();
            *self.rx.red_blocks.entry(shape.join(" ")).or_default() += 1;
            for b in &blocks[..blocks.len() - 1] {
                if b.data.is_empty() {
                    continue;
                }
                let t = time.wrapping_sub(u64::from(b.offset));
                if !self.rx.direct.contains_key(&t) {
                    self.rx.via_red.entry(t).or_insert(now);
                }
                // The redundant copy is a whole Opus frame, so it may carry LBRR
                // itself: a copy of the frame before it, two back from this packet.
                if b.data.len() > 1 && (b.data[0] >> 3) < 16 && b.data[0] & 0x03 == 0 && (b.data[1] >> 6) & 1 == 1 {
                    self.rx.via_red_lbrr.insert(t.wrapping_sub(960));
                }
            }
            primary = blocks[blocks.len() - 1].data.to_vec();
        } else {
            self.rx.plain_packets += 1;
            primary = p.payload.to_vec();
        }
        if primary.is_empty() {
            return;
        }
        // Opus: the TOC says the mode; in SILK and hybrid frames the second bit
        // of the first frame says whether it carries a copy of the frame before
        // (LBRR, the in-band FEC). `opus_packet_has_lbrr` reads the same bit.
        let config = primary[0] >> 3;
        let mode = match config {
            0..=11 => "silk",
            12..=15 => "hybrid",
            _ => "celt",
        };
        *self.rx.modes.entry(mode.into()).or_default() += 1;
        let frames_code = primary[0] & 0x03;
        let lbrr = config < 16 && frames_code == 0 && primary.len() > 1 && (primary[1] >> 6) & 1 == 1;
        let late = self.rx.direct.keys().next_back().is_some_and(|last| *last > time);
        self.rx.direct.insert(time, (now, lbrr, late));

        // Decode in arrival order and read the known signal back.
        let mut pcm = [0i16; 5760];
        if let Ok(n) = self.dec.decode(&primary, &mut pcm, false) {
            if self.rx.pcm.len() < 48_000 * 20 {
                self.rx.pcm.extend_from_slice(&pcm[..n]);
            }
            for block in pcm[..n].chunks_exact(signal::BLOCK) {
                let f: Vec<f32> = block.iter().map(|s| f32::from(*s) / 32768.0).collect();
                match signal::detect(&f) {
                    (Some(sym), _) => {
                        let ok = match self.rx.last_symbol {
                            None => true,
                            Some(prev) => sym == prev || sym == (prev + 1) % 256,
                        };
                        if ok {
                            self.rx.symbols_ok += 1;
                        } else {
                            self.rx.symbols_bad += 1;
                        }
                        self.rx.last_symbol = Some(sym);
                    }
                    (None, e) if e < 1e-5 => self.rx.blocks_silent += 1,
                    _ => {}
                }
            }
        }
    }

    /// Writes the 20 ms of audio that are due, and any key event.
    fn send_audio(&mut self, net: &Net) {
        let (Some(mid), Some(pt_opus)) = (self.mid, self.pt_opus) else { return };
        if self.connected_at.is_none() {
            return;
        }
        let t0 = *self.audio_t0.get_or_insert_with(Instant::now);
        let due = (t0.elapsed().as_micros() / 20_000) as u64;
        while self.tx_tick <= due {
            let tick = self.tx_tick;
            self.tx_tick += 1;
            if !self.sending {
                continue;
            }
            let ts = self.tx_ts0.wrapping_add((tick * signal::FRAME as u64) as u32);
            // A key event takes the place of audio while it lasts, as RFC 4733 has it.
            if self.tx_tele_gap > 0 {
                self.tx_tele_gap -= 1;
            } else if let (Some((event, start_ts, frames, _)), Some(pt_tele)) = (self.tx_tele.front().copied(), self.pt_tele) {
                let start_ts = if start_ts == 0 { ts } else { start_ts };
                let elapsed = (ts.wrapping_sub(start_ts) / signal::FRAME as u32) + 1;
                let end = elapsed >= frames;
                let units = (u64::from(elapsed) * 20 * u64::from(self.tele_rate) / 1000) as u16;
                let payload = [event, if end { 0x80 | 10 } else { 10 }, (units >> 8) as u8, units as u8];
                // On a payload type with an 8000 Hz clock: its own clock, or (ts48) the audio's.
                let rtp_time = if self.tx_tele_ts48 { start_ts } else { (u64::from(start_ts) * u64::from(self.tele_rate) / 48_000) as u32 };
                let repeats = if end { 3 } else { 1 };
                for _ in 0..repeats {
                    let seq = self.tx_seq;
                    self.tx_seq += 1;
                    if let Some(stream) = self.rtc.direct_api().stream_tx_by_mid(mid, None) {
                        stream.write_rtp(RtpWrite::new(pt_tele, seq.into(), rtp_time, Instant::now(), payload.to_vec()).marker(elapsed == 1));
                    }
                }
                if elapsed == 1 {
                    self.note("dtmf-out", json!({"event": event, "frames": frames}));
                }
                if end {
                    self.tx_tele.pop_front();
                    self.tx_tele_gap = 4;
                } else if let Some(front) = self.tx_tele.front_mut() {
                    front.1 = start_ts;
                }
                continue;
            }

            let mut pcm = [0i16; signal::FRAME];
            match self.opts.src.as_str() {
                "speech" if !net.speech.is_empty() => {
                    let at = (tick as usize * signal::FRAME) % net.speech.len();
                    for (i, s) in pcm.iter_mut().enumerate() {
                        *s = net.speech[(at + i) % net.speech.len()];
                    }
                }
                "silence" => {}
                _ => signal::tones(tick * signal::FRAME as u64, &mut pcm),
            }
            let mut out = [0u8; 1500];
            let Ok(n) = self.enc.encode(&pcm, &mut out) else { continue };
            let frame = out[..n].to_vec();
            let (pt, payload) = match self.pt_red {
                Some(pt_red) if self.opts.red => {
                    let mut redundant: Vec<(u32, &[u8])> = Vec::new();
                    let mut dist = self.opts.red_dist.clone();
                    dist.sort_unstable_by(|a, b| b.cmp(a));
                    for d in dist {
                        let want = ts.wrapping_sub(d * signal::FRAME as u32);
                        if let Some((_, data)) = self.tx_hist.iter().find(|(t, _)| *t == want) {
                            redundant.push((d * signal::FRAME as u32, data.as_slice()));
                        }
                    }
                    (pt_red, red::pack(*pt_opus, &redundant, &frame))
                }
                _ => (pt_opus, frame.clone()),
            };
            self.tx_hist.push_back((ts, frame));
            while self.tx_hist.len() > 8 {
                self.tx_hist.pop_front();
            }
            let seq = self.tx_seq;
            self.tx_seq += 1;
            self.tx_bytes += payload.len() as u64;
            let started = Instant::now();
            if let Some(stream) = self.rtc.direct_api().stream_tx_by_mid(mid, None) {
                stream.write_rtp(RtpWrite::new(pt, seq.into(), ts, Instant::now(), payload));
            }
            self.t_write.add(started.elapsed());
            self.tx_log.push((seq, ts, wall_ms(), false));
        }
    }

    fn stats(&self) -> String {
        let rx = &self.rx;
        let expected = rx.first_seq.map_or(0, |f| rx.max_seq - f + 1);
        // What became of each frame whose packet this endpoint dropped. Counting
        // from the drops, and not from holes in the RTP times, keeps a browser
        // that skipped frames by itself (it does when the machine is busy) out of it.
        let low = |t: &u64| *t as u32;
        let direct: HashMap<u32, (f64, bool, bool)> = rx.direct.iter().map(|(t, v)| (low(t), *v)).collect();
        let via_red: BTreeSet<u32> = rx.via_red.keys().map(low).collect();
        let via_red_lbrr: BTreeSet<u32> = rx.via_red_lbrr.iter().map(low).collect();
        let dropped_frames: BTreeSet<u32> = rx.dropped.values().map(|(_, ts)| *ts).collect();
        let missing_plain = dropped_frames.len() as u64;
        let (mut by_red, mut by_fec, mut by_either, mut by_deep, mut left_deep) = (0u64, 0u64, 0u64, 0u64, 0u64);
        // Left when a retransmission that came within so many ms also counts, on top of RED and FEC.
        let mut left_with_resend = [0u64; 4];
        let mut left_resend_only = [0u64; 4];
        let waits = [60.0, 100.0, 200.0, 1e9];
        for t in &dropped_frames {
            let red = via_red.contains(t);
            // The next packet arrived by itself (not as a later retransmission) and carries LBRR.
            let fec = direct.get(&t.wrapping_add(960)).is_some_and(|n| n.1 && !n.2);
            let deep = via_red_lbrr.contains(t);
            by_red += u64::from(red);
            by_fec += u64::from(fec);
            by_either += u64::from(red || fec);
            by_deep += u64::from(red || fec || deep);
            left_deep += u64::from(!(red || fec || deep));
            let resent = rx.resent_frames.get(t).copied();
            for (i, w) in waits.iter().enumerate() {
                let in_time = resent.is_some_and(|r| r <= *w);
                left_with_resend[i] += u64::from(!(red || fec || deep || in_time));
                left_resend_only[i] += u64::from(!in_time);
            }
        }
        let lbrr_frames = rx.direct.values().filter(|d| d.1).count() as u64;
        // Frames the browser sent: those that arrived by themselves and those dropped here.
        let span = rx.direct.keys().map(low).chain(dropped_frames.iter().copied()).collect::<BTreeSet<u32>>().len() as u64;
        // Frames the browser never sent: holes in the RTP times that are not our drops.
        let sender_gaps = match (rx.direct.keys().next(), rx.direct.keys().next_back()) {
            (Some(first), Some(last)) => ((last - first) / 960 + 1).saturating_sub(span),
            _ => 0,
        };
        let mut red_wait = Samples::default();
        let mut resend_wait: Vec<f64> = rx.resent.values().copied().collect();
        resend_wait.sort_by(|a, b| a.total_cmp(b));
        for (t, at) in &rx.via_red {
            // How long after the frame was due its redundant copy came: the packet after it.
            if let Some((next_at, _, _)) = rx.direct.get(&(t + 960)) {
                let _ = next_at;
            }
            let _ = at;
        }
        let _ = &mut red_wait;
        let pct = |p: f64| -> f64 {
            if resend_wait.is_empty() { 0.0 } else { resend_wait[((resend_wait.len() - 1) as f64 * p) as usize] }
        };
        let within = |ms: f64| resend_wait.iter().filter(|w| **w <= ms).count();
        let delivery = {
            let mut v = rx.frame_delivery_ms.clone();
            v.sort_by(|a, b| a.total_cmp(b));
            if v.is_empty() { json!(null) } else { json!({"n": v.len(), "p50_ms": v[v.len() / 2], "p99_ms": v[(v.len() - 1) * 99 / 100], "max_ms": v[v.len() - 1]}) }
        };
        let timing: BTreeMap<&str, Value> = self.t_in.iter().map(|(k, v)| (*k, v.json())).collect();
        let draining: BTreeMap<&str, Value> = self.t_drain.iter().map(|(k, v)| (*k, v.json())).collect();
        let whole: BTreeMap<&str, Value> = self.t_whole.iter().map(|(k, v)| (*k, v.json())).collect();
        json!({
            "id": self.id,
            "label": self.opts.label,
            "opts": format!("{:?}", self.opts),
            "created_wall_ms": self.created_wall,
            "connected_after_ms": self.connected_at,
            "dtls": self.dtls_version,
            "handshake_cpu_us": self.t_handshake.as_secs_f64() * 1e6,
            "remote": self.remote.map(|r| r.to_string()),
            "transport_packets_in": self.proto_seen,
            "negotiated": {"opus": self.pt_opus.map(|p| *p), "red": self.pt_red.map(|p| *p), "tele": self.pt_tele.map(|p| *p), "tele_rate": self.tele_rate},
            "rx": {
                "packets": rx.packets, "padding_only_packets": rx.padding, "bytes": rx.bytes, "expected_by_seq": expected,
                "payload_types": rx.pts, "red_packets": rx.red_packets, "plain_packets": rx.plain_packets,
                "red_shapes": rx.red_blocks, "opus_modes": rx.modes,
                "payload_bytes": {"mean": if rx.packets > 0 { rx.bytes as f64 / rx.packets as f64 } else { 0.0 }},
                "dropped_on_purpose": rx.dropped.len(), "dropped_again": rx.dropped_again,
                "resent_and_arrived": rx.resent.len(),
                "resend_wait_ms": {"p50": pct(0.5), "p90": pct(0.9), "p99": pct(0.99), "max": pct(1.0),
                                   "within_60ms": within(60.0), "within_100ms": within(100.0), "within_200ms": within(200.0)},
                "frames_in_span": span, "frames_missing_when_due": missing_plain, "frames_the_browser_never_sent": sender_gaps,
                "recoverable_by_red": by_red, "recoverable_by_fec": by_fec, "recoverable_by_red_or_fec": by_either,
                "recoverable_by_red_or_fec_or_the_lbrr_inside_a_redundant_block": by_deep, "left_after_all_three": left_deep,
                "left_after_all_three_and_a_resend_within_ms": {"60": left_with_resend[0], "100": left_with_resend[1], "200": left_with_resend[2], "any": left_with_resend[3]},
                "left_with_resends_alone_within_ms": {"60": left_resend_only[0], "100": left_resend_only[1], "200": left_resend_only[2], "any": left_resend_only[3]},
                "frames_with_lbrr": lbrr_frames, "frames_direct": rx.direct.len(),
                "signal": {"blocks_in_order": rx.symbols_ok, "blocks_out_of_order": rx.symbols_bad, "blocks_silent": rx.blocks_silent},
                "tele": rx.tele, "tele_packets_note": "[seq, rtp time, event, end, duration, pt, marker, wall ms]", "tele_packets": rx.tele_all, "red_malformed": rx.odd,
                "frame_mode": {"frames": rx.frames, "not_contiguous": rx.frames_noncontiguous, "handed_over_after": delivery},
            },
            "tx": {
                "packets": self.tx_log.len(), "payload_bytes_mean": if self.tx_log.is_empty() { 0.0 } else { self.tx_bytes as f64 / self.tx_log.len() as f64 },
                "dropped_on_purpose": self.tx_dropped.len(), "ts0": self.tx_ts0,
                "log": self.tx_log.iter().map(|(seq, ts, wall, _)| json!([seq, ts, wall, self.tx_dropped.contains(&(*seq as u16))])).collect::<Vec<_>>(),
                "stops": self.stop_log,
            },
            "cost": {"note": "time inside str0m only: handle_input, then poll_output until it asks for time; a packet in is the two together",
                     "handle_input": timing, "poll_output_after": draining, "packet_in_whole": whole,
                     "tick_with_a_frame_written": self.t_send.json(), "tick_with_nothing": self.t_idle.json(), "write_rtp": self.t_write.json()},
            "events": self.events,
            "trace_note": "the first packets that are not media: [ms since the offer, direction, kind, bytes, for DTLS the handshake message type of the first record]",
            "trace": self.trace,
        })
        .to_string()
    }
}

pub fn run(net: Net, cmds: Receiver<Cmd>) {
    let mut sessions: Vec<Sess> = Vec::new();
    let mut finished: HashMap<u32, String> = HashMap::new();
    let mut tcps: Vec<TcpConn> = Vec::new();
    let mut held_in: VecDeque<Held> = VecDeque::new();
    let mut held_out: VecDeque<Held> = VecDeque::new();
    let mut next_id = 1u32;
    let mut unclaimed: HashMap<(&'static str, SocketAddr), u64> = HashMap::new();
    let mut buf = vec![0u8; 2000];
    net.udp.set_nonblocking(true).ok();

    loop {
        let mut idle = true;

        // 1. What the HTTP side asks.
        while let Ok(cmd) = cmds.try_recv() {
            idle = false;
            match cmd {
                Cmd::Offer { sdp, opts, id, reply } => {
                    let result = match id.and_then(|id| sessions.iter_mut().find(|s| s.id == id)) {
                        Some(s) => {
                            s.note("re-offer", json!({}));
                            s.accept(&sdp).map(|a| (s.id, a))
                        }
                        None => Sess::new(next_id, &net, opts).and_then(|mut s| {
                            let answer = s.accept(&sdp)?;
                            let id = s.id;
                            next_id += 1;
                            sessions.push(s);
                            Ok((id, answer))
                        }),
                    };
                    let _ = reply.send(result);
                }
                Cmd::Control { id, q, reply } => {
                    let out = match sessions.iter_mut().find(|s| s.id == id) {
                        Some(s) => control(s, &q),
                        None => "no such session".into(),
                    };
                    let _ = reply.send(out);
                }
                Cmd::Stats { id, reply } => {
                    let out = sessions
                        .iter()
                        .find(|s| s.id == id)
                        .map(Sess::stats)
                        .or_else(|| finished.get(&id).cloned())
                        .unwrap_or_else(|| "{}".into());
                    let _ = reply.send(out);
                }
                Cmd::Tcp(stream, peer) => {
                    stream.set_nonblocking(true).ok();
                    stream.set_nodelay(true).ok();
                    tcps.push(TcpConn { stream, peer, buf: Vec::new(), dead: false });
                }
            }
        }

        // 2. What the network brought: datagrams, and frames of the TCP connections.
        let mut arrived: Vec<(Protocol, SocketAddr, Vec<u8>)> = Vec::new();
        loop {
            match net.udp.recv_from(&mut buf) {
                Ok((n, from)) => arrived.push((Protocol::Udp, from, buf[..n].to_vec())),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        for c in &mut tcps {
            loop {
                match c.stream.read(&mut buf) {
                    Ok(0) => {
                        c.dead = true;
                        break;
                    }
                    Ok(n) => c.buf.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => {
                        c.dead = true;
                        break;
                    }
                }
            }
            // RFC 4571: each packet behind two bytes of length.
            while c.buf.len() >= 2 {
                let len = usize::from(u16::from_be_bytes([c.buf[0], c.buf[1]]));
                if c.buf.len() < 2 + len {
                    break;
                }
                arrived.push((Protocol::Tcp, c.peer, c.buf[2..2 + len].to_vec()));
                c.buf.drain(..2 + len);
            }
        }
        tcps.retain(|c| !c.dead);
        let now = Instant::now();
        for (proto, peer, data) in arrived {
            idle = false;
            held_in.push_back(Held { at: now, proto, peer, data });
        }

        // 3. Hand each to the session that takes it, after the delay it was given.
        while let Some(h) = held_in.front() {
            let delay = sessions.iter().find(|s| s.remote == Some(h.peer)).or(sessions.last()).map_or(0, |s| s.opts.delay_ms);
            if h.at + Duration::from_millis(delay) > Instant::now() {
                break;
            }
            let Some(h) = held_in.pop_front() else { break };
            let local = if h.proto == Protocol::Tcp { net.tcp_addr } else { net.udp_addr };
            let Ok(receive) = Receive::new(h.proto, h.peer, local, &h.data) else { continue };
            let input = Input::Receive(Instant::now(), receive);
            let Some(s) = sessions.iter_mut().find(|s| s.rtc.accepts(&input)) else {
                let n = unclaimed.entry((kind_of(&h.data), h.peer)).or_insert(0u64);
                *n += 1;
                if *n == 1 || *n == 20 {
                    eprintln!("unclaimed {} from {} (x{n})", kind_of(&h.data), h.peer);
                }
                continue;
            };
            let kind = kind_of(&h.data);
            if h.proto == Protocol::Udp && s.opts.drop_udp {
                *s.proto_seen.entry("udp-dropped".into()).or_default() += 1;
                continue;
            }
            *s.proto_seen.entry(format!("{}-{kind}", if h.proto == Protocol::Tcp { "tcp" } else { "udp" })).or_default() += 1;
            s.remote = Some(h.peer);
            if h.proto == Protocol::Tcp {
                s.peer_tcp = Some(h.peer);
            }
            if kind == "rtp" && s.opts.loss_in > 0.0 && h.data[0] & 0x20 == 0 && s.rng_in.next() < s.opts.loss_in {
                let seq = u16::from_be_bytes([h.data[2], h.data[3]]);
                let pt = h.data[1] & 0x7f;
                if Some(pt) == s.pt_opus.map(|p| *p) || Some(pt) == s.pt_red.map(|p| *p) {
                    if s.rx.dropped.contains_key(&seq) {
                        s.rx.dropped_again += 1;
                    } else {
                        let ts = u32::from_be_bytes([h.data[4], h.data[5], h.data[6], h.data[7]]);
                        s.rx.dropped.insert(seq, (wall_ms(), ts));
                    }
                    continue;
                }
            }
            if kind != "rtp" && kind != "rtcp" && s.trace.len() < 120 {
                s.trace.push(json!([(s.created.elapsed().as_secs_f64() * 1e4).round() / 10.0, "in", kind, h.data.len(), h.data.get(13).copied().filter(|_| kind == "dtls")]));
            }
            let started = Instant::now();
            if let Err(e) = s.rtc.handle_input(input) {
                s.note("input-error", json!(e.to_string()));
            }
            let took = started.elapsed();
            s.t_in.entry(kind).or_default().add(took);
            if s.connected_at.is_none() {
                s.t_handshake += took;
            }
            let before = s.connected_at.is_none();
            let inside = drain(s, &net, &mut tcps, &mut held_out);
            s.t_drain.entry(kind).or_default().add(inside);
            s.t_whole.entry(kind).or_default().add(took + inside);
            if before {
                s.t_handshake += inside;
            }
        }

        // 4. Time: audio that is due, and str0m's own timers.
        for s in &mut sessions {
            let before = s.tx_log.len();
            s.send_audio(&net);
            let wrote = s.tx_log.len() > before;
            let started = Instant::now();
            let _ = s.rtc.handle_input(Input::Timeout(Instant::now()));
            let took = started.elapsed();
            let inside = drain(s, &net, &mut tcps, &mut held_out);
            if wrote {
                s.t_send.add(took + inside);
            } else {
                s.t_idle.add(took + inside);
            }
        }

        // 5. What was held back on the way out.
        while let Some(h) = held_out.front() {
            if h.at > Instant::now() {
                break;
            }
            let Some(h) = held_out.pop_front() else { break };
            put(&net, &mut tcps, h.proto, h.peer, &h.data);
        }

        for s in sessions.iter().filter(|s| s.dead || !s.rtc.is_alive()) {
            finished.insert(s.id, s.stats());
        }
        sessions.retain(|s| !(s.dead || !s.rtc.is_alive()));

        if idle {
            std::thread::sleep(Duration::from_micros(300));
        }
    }
}

fn put(net: &Net, tcps: &mut [TcpConn], proto: Protocol, peer: SocketAddr, data: &[u8]) {
    if proto == Protocol::Tcp {
        if let Some(c) = tcps.iter_mut().find(|c| c.peer == peer) {
            let mut framed = Vec::with_capacity(data.len() + 2);
            framed.extend_from_slice(&(data.len() as u16).to_be_bytes());
            framed.extend_from_slice(data);
            // Throwaway: a connection that cannot take a frame at once loses it.
            let _ = c.stream.write_all(&framed);
        }
    } else {
        let _ = net.udp.send_to(data, peer);
    }
}

/// Takes everything str0m has to say until it asks for time. Returns the time
/// spent inside str0m, without what this program does with an event.
fn drain(s: &mut Sess, net: &Net, tcps: &mut [TcpConn], held_out: &mut VecDeque<Held>) -> Duration {
    let mut inside = Duration::ZERO;
    loop {
        let started = Instant::now();
        let out = s.rtc.poll_output();
        inside += started.elapsed();
        match out {
            Ok(Output::Timeout(_)) => break,
            Ok(Output::Transmit(t)) => {
                let data: &[u8] = &t.contents;
                let k = kind_of(data);
                if k != "rtp" && k != "rtcp" && s.trace.len() < 120 {
                    s.trace.push(json!([(s.created.elapsed().as_secs_f64() * 1e4).round() / 10.0, "out", k, data.len(), data.get(13).copied().filter(|_| k == "dtls")]));
                }
                if kind_of(data) == "rtp" && s.opts.loss_out > 0.0 && s.rng_out.next() < s.opts.loss_out {
                    s.tx_dropped.insert(u16::from_be_bytes([data[2], data[3]]));
                    continue;
                }
                if s.opts.delay_ms > 0 {
                    held_out.push_back(Held { at: Instant::now() + Duration::from_millis(s.opts.delay_ms), proto: t.proto, peer: t.destination, data: data.to_vec() });
                } else {
                    put(net, tcps, t.proto, t.destination, data);
                }
            }
            Ok(Output::Event(e)) => s.on_event(e, net),
            Err(e) => {
                s.note("poll-error", json!(e.to_string()));
                s.dead = true;
                break;
            }
        }
    }
    inside
}

fn control(s: &mut Sess, q: &HashMap<String, String>) -> String {
    match q.get("cmd").map(String::as_str) {
        Some("stop") => {
            s.sending = false;
            let last = s.tx_log.last().copied();
            let v = json!({"cmd": "stop", "wall_ms": wall_ms(), "last_seq": last.map(|l| l.0), "last_ts": last.map(|l| l.1), "last_sent_wall_ms": last.map(|l| l.2)});
            s.stop_log.push(v.clone());
            v.to_string()
        }
        Some("start") => {
            s.sending = true;
            let v = json!({"cmd": "start", "wall_ms": wall_ms()});
            s.stop_log.push(v.clone());
            v.to_string()
        }
        Some("dtmf") => {
            s.tx_tele_ts48 = q.get("ts48").is_some_and(|v| v == "1");
            for c in q.get("digits").map(String::as_str).unwrap_or("1").chars() {
                let event = match c {
                    '0'..='9' => c as u8 - b'0',
                    '*' => 10,
                    '#' => 11,
                    'A'..='D' => c as u8 - b'A' + 12,
                    _ => continue,
                };
                // 160 ms of tone, then 80 ms of ordinary audio before the next.
                s.tx_tele.push_back((event, 0, 8, false));
            }
            json!({"cmd": "dtmf", "queued": s.tx_tele.len(), "pt": s.pt_tele.map(|p| *p)}).to_string()
        }
        Some("loss") => {
            if let Some(v) = q.get("in").and_then(|v| v.parse().ok()) {
                s.opts.loss_in = v;
            }
            if let Some(v) = q.get("out").and_then(|v| v.parse().ok()) {
                s.opts.loss_out = v;
            }
            if let Some(v) = q.get("fec").and_then(|v| v.parse::<i32>().ok()) {
                let _ = s.enc.set_inband_fec(v > 0);
                let _ = s.enc.set_packet_loss_perc(v);
                s.opts.fec = v;
            }
            json!({"cmd": "loss", "in": s.opts.loss_in, "out": s.opts.loss_out, "fec": s.opts.fec}).to_string()
        }
        Some("wav") => {
            let path = q.get("path").cloned().unwrap_or_default();
            let ok = write_wav(&path, &s.rx.pcm).is_ok();
            json!({"cmd": "wav", "ok": ok, "samples": s.rx.pcm.len()}).to_string()
        }
        Some("close") => {
            s.rtc.disconnect();
            "{}".into()
        }
        _ => "unknown cmd".into(),
    }
}

fn write_wav(path: &str, pcm: &[i16]) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    let data_len = (pcm.len() * 2) as u32;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_len).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?;
    f.write_all(&48_000u32.to_le_bytes())?;
    f.write_all(&96_000u32.to_le_bytes())?;
    f.write_all(&2u16.to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;
    for s in pcm {
        f.write_all(&s.to_le_bytes())?;
    }
    Ok(())
}
