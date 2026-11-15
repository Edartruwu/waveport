//! The density harness: synthetic voice sessions on pinned shards.
//!
//! Each session is one task on a shard's single-threaded runtime. It decodes
//! a real Opus packet every 20 ms, cuts the audio into voice-activity frames
//! and hands them to a batching inference thread, encodes outbound Opus
//! while its agent "speaks", handles synthetic provider messages, and asks
//! for one end-of-turn inference per ten-second conversation cycle.
//!
//! ```sh
//! cargo run --release --bin density -- --sessions 100 --shard-cpus 6 \
//!     --batcher-cpus 8 --turn-cpus 10 --generator-cpus 12
//! ```

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use density_spike::backend::{
    self, Kind, VAD_CONTEXT, VAD_FRAME, VAD_INPUT, VAD_STATE, VAD_STATE_WIDTH,
};
use density_spike::mel::{FEATURES, MelFrontEnd};
use density_spike::stats::{Histogram, Rng, calibrate, rss_bytes, thread_sched};
use density_spike::{Options, Result, audio, pin_to};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

/// Packets in one conversation cycle: ten seconds of 20 ms packets. The
/// user speaks for the first four seconds.
const CYCLE: u16 = 500;
/// The end-of-turn inference is requested 0.2 s after the user stops.
const TURN_AT: u16 = 210;
/// The agent speaks from 5.0 s to 9.5 s of each cycle: 45% of the time.
const AGENT: std::ops::Range<u16> = 250..475;

const TICK: Duration = Duration::from_millis(5);
const FRAME: Duration = Duration::from_millis(20);
const MAILBOX: usize = 256;
const MAX_BATCH: usize = 64;
const BATCH_WINDOW: Duration = Duration::from_millis(2);
const POLL: Duration = Duration::from_micros(250);
const VAD_RING: usize = 1024;

/// Per-session buffers, sized as the design sizes them.
const PREROLL: usize = 8 * 16_000;
const STT_RING: usize = 2 * 16_000;
const PLAYOUT: usize = 2 * 24_000;
const KERNEL: usize = 32 * 1024;
const RETAINED: usize = 64;
/// Frames quieter than this are skipped when the energy gate is on.
const GATE_RMS: f32 = 0.004;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Policy {
    /// Run when 64 frames are waiting or the oldest is 2 ms old.
    Window,
    /// Run as soon as anything is waiting.
    Greedy,
}

struct Config {
    label: String,
    backend: Kind,
    vad_model: PathBuf,
    turn_model: PathBuf,
    shard_cpus: Vec<usize>,
    batcher_cpus: Vec<usize>,
    turn_cpus: Vec<usize>,
    generator_cpus: Vec<usize>,
    sessions: usize,
    warmup: Duration,
    measure: Duration,
    policy: Policy,
    complexity: i32,
    /// Fraction of the sessions replaced each minute.
    churn: f64,
    energy_gate: bool,
}

struct Assets {
    user_packets: Vec<Box<[u8]>>,
    agent_pcm: Vec<i16>,
}

enum Msg {
    Inbound { seq: u16, sched: Instant },
    Provider { size: u16, retain: bool },
    Pace,
    Vad { prob: f32 },
}

struct Envelope {
    slot: u32,
    msg: Msg,
}

enum Control {
    Begin,
    End,
    Replace(u32),
    TurnDone { requested: Instant },
    Shutdown,
}

struct VadJob {
    slot: u32,
    reset: bool,
    pushed: Instant,
    input: [f32; VAD_INPUT],
}

struct VadResult {
    slot: u32,
    prob: f32,
    pushed: Instant,
    done: Instant,
}

struct TurnJob {
    shard: usize,
    requested: Instant,
    audio: Vec<i16>,
}

#[derive(Default, Clone)]
struct Stats {
    tick_late: Histogram,
    ingress_delay: Histogram,
    decode: Histogram,
    encode: Histogram,
    provider: Histogram,
    vad_compute: Histogram,
    vad_delivered: Histogram,
    turn_latency: Histogram,
    start_initial: Histogram,
    start_churn: Histogram,
    slow_handlers: u64,
    vad_pushed: u64,
    vad_dropped: u64,
    vad_gated: u64,
    vad_results: u64,
    vad_speech: u64,
    turn_requests: u64,
    turn_results: u64,
    mailbox_overflow: u64,
    pace_resyncs: u64,
    encoded_bytes: u64,
}

impl Stats {
    /// Forgets everything recorded during warm-up, except how long the
    /// first sessions took to start.
    fn begin(&mut self) {
        let start_initial = std::mem::take(&mut self.start_initial);
        *self = Self::default();
        self.start_initial = start_initial;
    }

    fn merge(&mut self, other: &Self) {
        self.tick_late.merge(&other.tick_late);
        self.ingress_delay.merge(&other.ingress_delay);
        self.decode.merge(&other.decode);
        self.encode.merge(&other.encode);
        self.provider.merge(&other.provider);
        self.vad_compute.merge(&other.vad_compute);
        self.vad_delivered.merge(&other.vad_delivered);
        self.turn_latency.merge(&other.turn_latency);
        self.start_initial.merge(&other.start_initial);
        self.start_churn.merge(&other.start_churn);
        self.slow_handlers += other.slow_handlers;
        self.vad_pushed += other.vad_pushed;
        self.vad_dropped += other.vad_dropped;
        self.vad_gated += other.vad_gated;
        self.vad_results += other.vad_results;
        self.vad_speech += other.vad_speech;
        self.turn_requests += other.turn_requests;
        self.turn_results += other.turn_results;
        self.mailbox_overflow += other.mailbox_overflow;
        self.pace_resyncs += other.pace_resyncs;
        self.encoded_bytes += other.encoded_bytes;
    }
}

struct ShardReport {
    shard: usize,
    wall: Duration,
    run_ns: u64,
    wait_ns: u64,
    stats: Stats,
}

/// What a shard's tasks share. Everything here stays on the shard's thread.
struct Shared {
    shard: usize,
    config: Arc<Config>,
    assets: Arc<Assets>,
    stats: RefCell<Stats>,
    vad_tx: RefCell<rtrb::Producer<VadJob>>,
    turn_tx: std_mpsc::Sender<TurnJob>,
    socket: UdpSocket,
}

struct Session {
    slot: u32,
    decoder: opus::Decoder,
    encoder: opus::Encoder,
    vad_pending: Vec<f32>,
    vad_context: [f32; VAD_CONTEXT],
    fresh: bool,
    preroll: Vec<i16>,
    preroll_at: usize,
    stt: Vec<i16>,
    stt_at: usize,
    playout: Vec<i16>,
    playout_at: usize,
    kernel: Vec<u8>,
    retained: VecDeque<Vec<u8>>,
    packet: Vec<u8>,
    agent_at: usize,
    seq: u16,
    rng: Rng,
}

fn ring_write(ring: &mut [i16], at: &mut usize, data: &[i16]) {
    let first = data.len().min(ring.len() - *at);
    ring[*at..*at + first].copy_from_slice(&data[..first]);
    let rest = data.len() - first;
    ring[..rest].copy_from_slice(&data[first..]);
    *at = (*at + data.len()) % ring.len();
}

impl Session {
    fn new(slot: u32, shared: &Shared) -> Result<Self> {
        let encoder = audio::encoder(audio::EncodeSettings {
            rate: 24_000,
            bitrate: 32_000,
            complexity: shared.config.complexity,
            force_fullband: false,
        })?;
        Ok(Self {
            slot,
            decoder: opus::Decoder::new(16_000, opus::Channels::Mono)?,
            encoder,
            vad_pending: Vec::with_capacity(2 * VAD_FRAME),
            vad_context: [0.0; VAD_CONTEXT],
            fresh: true,
            preroll: vec![0; PREROLL],
            preroll_at: 0,
            stt: vec![0; STT_RING],
            stt_at: 0,
            playout: vec![0; PLAYOUT],
            playout_at: 0,
            kernel: vec![0; KERNEL],
            retained: VecDeque::with_capacity(RETAINED + 1),
            packet: vec![0; 1500],
            agent_at: (slot as usize * 4_800) % shared.assets.agent_pcm.len(),
            seq: 0,
            rng: Rng::new(u64::from(slot) << 8 | shared.shard as u64),
        })
    }

    fn handle(&mut self, msg: Msg, shared: &Shared) {
        let start = Instant::now();
        match msg {
            Msg::Inbound { seq, sched } => {
                let (pushed, dropped, gated, turn) = self.on_inbound(seq, shared);
                let elapsed = start.elapsed();
                let mut stats = shared.stats.borrow_mut();
                stats
                    .ingress_delay
                    .record_duration(start.saturating_duration_since(sched));
                stats.decode.record_duration(elapsed);
                stats.vad_pushed += pushed;
                stats.vad_dropped += dropped;
                stats.vad_gated += gated;
                stats.turn_requests += turn;
                stats.slow_handlers += u64::from(elapsed > Duration::from_micros(100));
            }
            Msg::Provider { size, retain } => {
                self.on_provider(size, retain);
                let elapsed = start.elapsed();
                let mut stats = shared.stats.borrow_mut();
                stats.provider.record_duration(elapsed);
                stats.slow_handlers += u64::from(elapsed > Duration::from_micros(100));
            }
            Msg::Pace => {
                if let Some(bytes) = self.on_pace(shared) {
                    let elapsed = start.elapsed();
                    let mut stats = shared.stats.borrow_mut();
                    stats.encode.record_duration(elapsed);
                    stats.encoded_bytes += bytes as u64;
                    stats.slow_handlers += u64::from(elapsed > Duration::from_micros(100));
                }
            }
            Msg::Vad { prob } => {
                // The kernel would take a speech edge from this; here it
                // only lands in session state.
                let at = self.kernel.len() - 1;
                self.kernel[at] = u8::from(prob > 0.5);
            }
        }
    }

    /// Decodes one packet, fills the rings and emits voice-activity frames.
    /// Returns frames pushed, dropped and gated, and turn requests made.
    fn on_inbound(&mut self, seq: u16, shared: &Shared) -> (u64, u64, u64, u64) {
        self.seq = seq;
        let mut pcm = [0_i16; 320];
        let packet = &shared.assets.user_packets[usize::from(seq)];
        let samples = self.decoder.decode(packet, &mut pcm, false).unwrap_or(0);
        let pcm = &pcm[..samples];
        ring_write(&mut self.preroll, &mut self.preroll_at, pcm);
        ring_write(&mut self.stt, &mut self.stt_at, pcm);
        self.vad_pending
            .extend(pcm.iter().map(|&s| f32::from(s) / 32768.0));

        let (mut pushed, mut dropped, mut gated) = (0, 0, 0);
        while self.vad_pending.len() >= VAD_FRAME {
            let frame = &self.vad_pending[..VAD_FRAME];
            let quiet = shared.config.energy_gate && {
                let power: f32 = frame.iter().map(|x| x * x).sum();
                (power / VAD_FRAME as f32).sqrt() < GATE_RMS
            };
            if quiet {
                gated += 1;
            } else {
                let mut job = VadJob {
                    slot: self.slot,
                    reset: self.fresh,
                    pushed: Instant::now(),
                    input: [0.0; VAD_INPUT],
                };
                job.input[..VAD_CONTEXT].copy_from_slice(&self.vad_context);
                job.input[VAD_CONTEXT..].copy_from_slice(frame);
                if shared.vad_tx.borrow_mut().push(job).is_ok() {
                    pushed += 1;
                    self.fresh = false;
                } else {
                    dropped += 1;
                }
            }
            self.vad_context
                .copy_from_slice(&frame[VAD_FRAME - VAD_CONTEXT..]);
            self.vad_pending.drain(..VAD_FRAME);
        }

        let mut turn = 0;
        if seq == TURN_AT && !shared.config.turn_cpus.is_empty() {
            // The last eight seconds, oldest first.
            let mut audio = Vec::with_capacity(PREROLL);
            audio.extend_from_slice(&self.preroll[self.preroll_at..]);
            audio.extend_from_slice(&self.preroll[..self.preroll_at]);
            let job = TurnJob {
                shard: shared.shard,
                requested: Instant::now(),
                audio,
            };
            if shared.turn_tx.send(job).is_ok() {
                turn = 1;
            }
        }
        (pushed, dropped, gated, turn)
    }

    /// Encodes and sends one outbound frame while the agent is speaking.
    fn on_pace(&mut self, shared: &Shared) -> Option<usize> {
        if !AGENT.contains(&self.seq) {
            return None;
        }
        let pcm = &shared.assets.agent_pcm;
        if self.agent_at + 480 > pcm.len() {
            self.agent_at = 0;
        }
        let frame = &pcm[self.agent_at..self.agent_at + 480];
        self.agent_at += 480;
        ring_write(&mut self.playout, &mut self.playout_at, frame);
        let bytes = self.encoder.encode(frame, &mut self.packet).unwrap_or(0);
        // A real send: one system call per packet, as on a media socket.
        let _ = shared.socket.send(&self.packet[..bytes]);
        Some(bytes)
    }

    /// Stands in for a message from a provider connection: a buffer is
    /// allocated and filled as if read from a socket, scanned twice as if
    /// framed and parsed, and then either dropped or kept as context.
    fn on_provider(&mut self, size: u16, retain: bool) {
        let mut buffer = vec![0_u8; usize::from(size)];
        let mut word = self.rng.next();
        for chunk in buffer.chunks_mut(8) {
            word = word.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(17);
            chunk.copy_from_slice(&word.to_le_bytes()[..chunk.len()]);
        }
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for _ in 0..2 {
            for &byte in &buffer {
                hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
            }
        }
        let at = (hash as usize) % (KERNEL - 1);
        self.kernel[at] = self.kernel[at].wrapping_add(1);
        if retain {
            self.retained.push_back(buffer);
            if self.retained.len() > RETAINED {
                self.retained.pop_front();
            }
        }
    }
}

type Senders = Rc<RefCell<Vec<mpsc::Sender<Msg>>>>;

/// Creates the session in `slot`, replacing the one there. The old task
/// ends when it has drained its mailbox, and frees its session.
fn start_session(slot: usize, shared: &Rc<Shared>, senders: &Senders, initial: bool) -> Result<()> {
    let start = Instant::now();
    let mut session = Session::new(slot as u32, shared)?;
    let (tx, mut rx) = mpsc::channel::<Msg>(MAILBOX);
    let task_shared = Rc::clone(shared);
    tokio::task::spawn_local(async move {
        while let Some(msg) = rx.recv().await {
            session.handle(msg, &task_shared);
        }
    });
    let mut senders = senders.borrow_mut();
    if slot < senders.len() {
        senders[slot] = tx;
    } else {
        senders.push(tx);
    }
    let elapsed = start.elapsed();
    let mut stats = shared.stats.borrow_mut();
    if initial {
        stats.start_initial.record_duration(elapsed);
    } else {
        stats.start_churn.record_duration(elapsed);
    }
    Ok(())
}

struct ShardIo {
    ingress: mpsc::Receiver<Envelope>,
    control: mpsc::UnboundedReceiver<Control>,
    vad_tx: rtrb::Producer<VadJob>,
    vad_rx: rtrb::Consumer<VadResult>,
    turn_tx: std_mpsc::Sender<TurnJob>,
    sink: SocketAddr,
    reports: std_mpsc::Sender<ShardReport>,
    ready: std_mpsc::Sender<usize>,
}

fn shard_thread(
    index: usize,
    cpu: usize,
    config: Arc<Config>,
    assets: Arc<Assets>,
    io: ShardIo,
) -> Result<(Duration, Duration)> {
    pin_to(cpu)?;
    let calibration_before = calibrate();
    let socket = UdpSocket::bind("127.0.0.1:0")?;
    socket.connect(io.sink)?;
    socket.set_nonblocking(true)?;
    let sessions = config.sessions;
    let shared = Rc::new(Shared {
        shard: index,
        config,
        assets,
        stats: RefCell::new(Stats::default()),
        vad_tx: RefCell::new(io.vad_tx),
        turn_tx: io.turn_tx,
        socket,
    });
    let (mut ingress, mut control, mut vad_rx) = (io.ingress, io.control, io.vad_rx);
    let (reports, ready) = (io.reports, io.ready);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build_local(tokio::runtime::LocalOptions::default())?;
    runtime.block_on(async {
        let senders: Senders = Rc::new(RefCell::new(Vec::with_capacity(sessions)));
        for slot in 0..sessions {
            start_session(slot, &shared, &senders, true)?;
        }
        let _ = ready.send(index);

        // The router: what a shard's media socket does, one hop from the
        // load generator to the session that owns the packet.
        let router_senders = Rc::clone(&senders);
        let router_shared = Rc::clone(&shared);
        tokio::task::spawn_local(async move {
            let mut buffer = Vec::with_capacity(512);
            while ingress.recv_many(&mut buffer, 512).await > 0 {
                let mut overflow = 0;
                {
                    let senders = router_senders.borrow();
                    for envelope in buffer.drain(..) {
                        if let Some(tx) = senders.get(envelope.slot as usize) {
                            overflow += u64::from(tx.try_send(envelope.msg).is_err());
                        }
                    }
                }
                if overflow > 0 {
                    router_shared.stats.borrow_mut().mailbox_overflow += overflow;
                }
            }
        });

        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let begin = Instant::now();
        let mut next_pace: Vec<Instant> = (0..sessions)
            .map(|slot| begin + TICK * (slot % 4) as u32)
            .collect();
        let mut mark: Option<(Instant, u64, u64)> = None;
        let mut next_replace = 0_usize;
        loop {
            let scheduled = interval.tick().await.into_std();
            let now = Instant::now();
            shared
                .stats
                .borrow_mut()
                .tick_late
                .record_duration(now.saturating_duration_since(scheduled));

            let mut stop = false;
            while let Ok(message) = control.try_recv() {
                match message {
                    Control::Begin => {
                        shared.stats.borrow_mut().begin();
                        let (run, wait) = thread_sched();
                        mark = Some((Instant::now(), run, wait));
                    }
                    Control::End => {
                        let (run, wait) = thread_sched();
                        let (since, run0, wait0) = mark.unwrap_or((begin, 0, 0));
                        let _ = reports.send(ShardReport {
                            shard: index,
                            wall: since.elapsed(),
                            run_ns: run - run0,
                            wait_ns: wait - wait0,
                            stats: shared.stats.borrow().clone(),
                        });
                    }
                    Control::Replace(count) => {
                        for _ in 0..count {
                            start_session(next_replace % sessions, &shared, &senders, false)?;
                            next_replace += 1;
                        }
                    }
                    Control::TurnDone { requested } => {
                        let mut stats = shared.stats.borrow_mut();
                        stats.turn_results += 1;
                        stats
                            .turn_latency
                            .record_duration(now.saturating_duration_since(requested));
                    }
                    Control::Shutdown => stop = true,
                }
            }
            if stop {
                break;
            }

            let senders = senders.borrow();
            let mut stats = shared.stats.borrow_mut();
            while let Ok(result) = vad_rx.pop() {
                stats.vad_results += 1;
                stats.vad_speech += u64::from(result.prob > 0.5);
                stats
                    .vad_compute
                    .record_duration(result.done.saturating_duration_since(result.pushed));
                stats
                    .vad_delivered
                    .record_duration(now.saturating_duration_since(result.pushed));
                if let Some(tx) = senders.get(result.slot as usize) {
                    stats.mailbox_overflow +=
                        u64::from(tx.try_send(Msg::Vad { prob: result.prob }).is_err());
                }
            }
            for (slot, due) in next_pace.iter_mut().enumerate() {
                if *due <= now {
                    stats.mailbox_overflow += u64::from(senders[slot].try_send(Msg::Pace).is_err());
                    *due += FRAME;
                    if *due + 5 * FRAME < now {
                        // Hopelessly late: start again from now.
                        *due = now + FRAME;
                        stats.pace_resyncs += 1;
                    }
                }
            }
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })?;
    drop(runtime);
    Ok((calibration_before, calibrate()))
}

/// Counters a worker thread publishes for the main thread to read.
#[derive(Default)]
struct Gauge {
    run_ns: AtomicU64,
    wait_ns: AtomicU64,
    jobs: AtomicU64,
    items: AtomicU64,
}

impl Gauge {
    fn publish(&self) {
        let (run, wait) = thread_sched();
        self.run_ns.store(run, Ordering::Relaxed);
        self.wait_ns.store(wait, Ordering::Relaxed);
    }

    fn read(&self) -> [u64; 4] {
        [
            self.run_ns.load(Ordering::Relaxed),
            self.wait_ns.load(Ordering::Relaxed),
            self.jobs.load(Ordering::Relaxed),
            self.items.load(Ordering::Relaxed),
        ]
    }
}

struct BatcherLink {
    jobs: rtrb::Consumer<VadJob>,
    results: rtrb::Producer<VadResult>,
    states: Vec<[f32; VAD_STATE]>,
    carried: Option<VadJob>,
}

struct Worker {
    gauge: Arc<Gauge>,
    stop: Arc<AtomicBool>,
    /// Bumped when the measurement starts; the worker then forgets what it
    /// recorded during warm-up.
    epoch: Arc<AtomicU64>,
    ready: std_mpsc::Sender<usize>,
}

fn batcher_thread(
    cpu: usize,
    config: Arc<Config>,
    mut links: Vec<BatcherLink>,
    worker: Worker,
) -> Result<Histogram> {
    pin_to(cpu)?;
    let mut vad = backend::load_vad(config.backend, &config.vad_model)?;
    let mut input = vec![0.0_f32; MAX_BATCH * VAD_INPUT];
    let mut state = vec![0.0_f32; MAX_BATCH * VAD_STATE];
    let mut probs = vec![0.0_f32; MAX_BATCH];
    // Touch every batch size once, so that no plan is built mid-run.
    for batch in 1..=MAX_BATCH {
        state[..batch * VAD_STATE].fill(0.0);
        vad.run(
            batch,
            &input[..batch * VAD_INPUT],
            &mut state[..batch * VAD_STATE],
            &mut probs[..batch],
        )?;
    }
    let _ = worker.ready.send(cpu);

    let width = VAD_STATE_WIDTH;
    let mut batch: Vec<(usize, VadJob)> = Vec::with_capacity(MAX_BATCH);
    let mut inference = Histogram::new();
    let mut first = 0;
    let mut published = Instant::now();
    let mut epoch = 0;
    while !worker.stop.load(Ordering::Relaxed) {
        let count = links.len();
        for k in 0..count {
            let at = (first + k) % count;
            let link = &mut links[at];
            while batch.len() < MAX_BATCH {
                let job = match link.carried.take() {
                    Some(job) => job,
                    None => match link.jobs.pop() {
                        Ok(job) => job,
                        Err(_) => break,
                    },
                };
                // Two frames of one stream depend on each other through
                // the state, so they cannot share a batch.
                if batch.iter().any(|(l, j)| *l == at && j.slot == job.slot) {
                    link.carried = Some(job);
                    break;
                }
                batch.push((at, job));
            }
        }
        first = (first + 1) % count.max(1);

        let now = Instant::now();
        if now.duration_since(published) >= Duration::from_millis(100) {
            worker.gauge.publish();
            published = now;
            let current = worker.epoch.load(Ordering::Relaxed);
            if current != epoch {
                epoch = current;
                inference.clear();
            }
        }
        if batch.is_empty() {
            std::thread::sleep(POLL);
            continue;
        }
        if config.policy == Policy::Window && batch.len() < MAX_BATCH {
            let oldest = batch.iter().map(|(_, job)| job.pushed).min().unwrap_or(now);
            let age = now.saturating_duration_since(oldest);
            if age < BATCH_WINDOW {
                std::thread::sleep((BATCH_WINDOW - age).min(POLL));
                continue;
            }
        }

        let n = batch.len();
        for (b, (at, job)) in batch.iter().enumerate() {
            input[b * VAD_INPUT..(b + 1) * VAD_INPUT].copy_from_slice(&job.input);
            let kept = &mut links[*at].states[job.slot as usize];
            if job.reset {
                kept.fill(0.0);
            }
            // The model lays state out as [layer, stream, width].
            state[b * width..(b + 1) * width].copy_from_slice(&kept[..width]);
            state[(n + b) * width..(n + b + 1) * width].copy_from_slice(&kept[width..]);
        }
        let start = Instant::now();
        vad.run(
            n,
            &input[..n * VAD_INPUT],
            &mut state[..n * VAD_STATE],
            &mut probs[..n],
        )?;
        let done = Instant::now();
        inference.record_duration(done.duration_since(start));
        for (b, (at, job)) in batch.drain(..).enumerate() {
            let link = &mut links[at];
            let kept = &mut link.states[job.slot as usize];
            kept[..width].copy_from_slice(&state[b * width..(b + 1) * width]);
            kept[width..].copy_from_slice(&state[(n + b) * width..(n + b + 1) * width]);
            let _ = link.results.push(VadResult {
                slot: job.slot,
                prob: probs[b],
                pushed: job.pushed,
                done,
            });
        }
        worker.gauge.jobs.fetch_add(1, Ordering::Relaxed);
        worker.gauge.items.fetch_add(n as u64, Ordering::Relaxed);
    }
    worker.gauge.publish();
    Ok(inference)
}

fn turn_thread(
    cpu: usize,
    config: Arc<Config>,
    jobs: Arc<Mutex<std_mpsc::Receiver<TurnJob>>>,
    controls: Vec<mpsc::UnboundedSender<Control>>,
    worker: Worker,
) -> Result<(Histogram, Histogram)> {
    pin_to(cpu)?;
    let mut model = backend::load_turn(config.backend, &config.turn_model)?;
    let mut front_end = MelFrontEnd::new();
    let mut features = vec![0.0_f32; FEATURES];
    let mut samples = vec![0.0_f32; PREROLL];
    front_end.extract(&samples, &mut features);
    model.run(&features)?;
    let _ = worker.ready.send(cpu);

    let (mut feature_time, mut inference_time) = (Histogram::new(), Histogram::new());
    let mut epoch = 0;
    while !worker.stop.load(Ordering::Relaxed) {
        let received = {
            let Ok(receiver) = jobs.lock() else { break };
            receiver.recv_timeout(Duration::from_millis(100))
        };
        let current = worker.epoch.load(Ordering::Relaxed);
        if current != epoch {
            epoch = current;
            feature_time.clear();
            inference_time.clear();
        }
        match received {
            Ok(job) => {
                let start = Instant::now();
                for (out, &sample) in samples.iter_mut().zip(&job.audio) {
                    *out = f32::from(sample) / 32768.0;
                }
                front_end.extract(&samples, &mut features);
                let middle = Instant::now();
                std::hint::black_box(model.run(&features)?);
                let end = Instant::now();
                feature_time.record_duration(middle.duration_since(start));
                inference_time.record_duration(end.duration_since(middle));
                if let Some(control) = controls.get(job.shard) {
                    let _ = control.send(Control::TurnDone {
                        requested: job.requested,
                    });
                }
                worker.gauge.jobs.fetch_add(1, Ordering::Relaxed);
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
        }
        worker.gauge.publish();
    }
    worker.gauge.publish();
    Ok((feature_time, inference_time))
}

/// Feeds one shard: a packet and a provider message per session every
/// 20 ms, with the sessions spread evenly over the 20 ms.
fn generator_thread(
    shard: usize,
    cpu: usize,
    sessions: usize,
    ingress: mpsc::Sender<Envelope>,
    go: Instant,
    stop: Arc<AtomicBool>,
) -> u64 {
    let _ = pin_to(cpu);
    let mut rng = Rng::new(0xfeed ^ shard as u64);
    let offsets: Vec<u64> = (0..sessions)
        .map(|slot| Rng::new((shard as u64) << 32 | slot as u64).below(u64::from(CYCLE)))
        .collect();
    let mut overflow = 0;
    let mut ms = 0_u64;
    while !stop.load(Ordering::Relaxed) {
        let due = go + Duration::from_millis(ms);
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        for slot in ((ms % 20) as usize..sessions).step_by(20) {
            let seq = ((ms / 20 + offsets[slot]) % u64::from(CYCLE)) as u16;
            let msg = Msg::Inbound { seq, sched: due };
            overflow += u64::from(
                ingress
                    .try_send(Envelope {
                        slot: slot as u32,
                        msg,
                    })
                    .is_err(),
            );
        }
        for slot in (((ms + 10) % 20) as usize..sessions).step_by(20) {
            let msg = Msg::Provider {
                size: 100 + rng.below(3_900) as u16,
                retain: rng.below(10) == 0,
            };
            overflow += u64::from(
                ingress
                    .try_send(Envelope {
                        slot: slot as u32,
                        msg,
                    })
                    .is_err(),
            );
        }
        ms += 1;
    }
    overflow
}

fn cpu_list(text: &str) -> Result<Vec<usize>> {
    if text == "none" {
        return Ok(Vec::new());
    }
    text.split(',')
        .map(|part| {
            part.parse()
                .map_err(|_| format!("invalid CPU list: {text}").into())
        })
        .collect()
}

fn config(options: &Options) -> Result<Config> {
    let backend: Kind = options.parse("--backend", Kind::Ort)?;
    let assets = PathBuf::from(options.value("--assets").unwrap_or("../../target/assets"));
    let vad_model = match backend {
        Kind::Ort => "silero-vad-v6.2.3.onnx",
        // `tract` cannot analyse the control flow of the standard export.
        Kind::Tract => "silero-vad-v6.2.3-ifless.onnx",
    };
    Ok(Config {
        label: options.value("--label").unwrap_or("run").to_owned(),
        backend,
        vad_model: assets.join(options.value("--vad-model").unwrap_or(vad_model)),
        turn_model: assets.join("smart-turn-v3.2-cpu.onnx"),
        shard_cpus: cpu_list(options.value("--shard-cpus").unwrap_or("6"))?,
        batcher_cpus: cpu_list(options.value("--batcher-cpus").unwrap_or("8"))?,
        turn_cpus: cpu_list(options.value("--turn-cpus").unwrap_or("10"))?,
        generator_cpus: cpu_list(options.value("--generator-cpus").unwrap_or("12,13,14,15"))?,
        sessions: options.parse("--sessions", 100)?,
        warmup: Duration::from_secs(options.parse("--warmup", 15)?),
        measure: Duration::from_secs(options.parse("--measure", 60)?),
        policy: match options.value("--policy").unwrap_or("window") {
            "window" => Policy::Window,
            "greedy" => Policy::Greedy,
            other => return Err(format!("unknown policy `{other}`").into()),
        },
        complexity: options.parse("--complexity", 10)?,
        churn: options.parse("--churn", 0.05)?,
        energy_gate: options.has("--energy-gate"),
    })
}

fn assets(options: &Options) -> Result<Assets> {
    let dir = PathBuf::from(options.value("--audio").unwrap_or("../../target/spike"));
    let user = audio::read_wav(&dir.join("user48k.wav"))?;
    if user.rate != 48_000 || user.samples.len() < usize::from(CYCLE) * 960 {
        return Err("user48k.wav must hold ten seconds at 48 kHz".into());
    }
    // What a browser sends for a real microphone: fullband, 32 kbit/s.
    let user_packets = audio::encode_packets(
        &user.samples[..usize::from(CYCLE) * 960],
        audio::EncodeSettings {
            rate: 48_000,
            bitrate: 32_000,
            complexity: 10,
            force_fullband: true,
        },
    )?;
    let agent = audio::read_wav(&dir.join("speech24k.wav"))?;
    if agent.rate != 24_000 {
        return Err("speech24k.wav must be at 24 kHz".into());
    }
    Ok(Assets {
        user_packets,
        agent_pcm: agent.samples,
    })
}

fn percent(part: u64, whole: Duration) -> f64 {
    100.0 * part as f64 / whole.as_nanos().max(1) as f64
}

fn main() -> Result<()> {
    let options = Options::from_env();
    let config = Arc::new(config(&options)?);
    let assets = Arc::new(assets(&options)?);
    let shards = config.shard_cpus.len();
    let total_sessions = shards * config.sessions;
    let stop = Arc::new(AtomicBool::new(false));
    let epoch = Arc::new(AtomicU64::new(0));
    let sink = UdpSocket::bind("127.0.0.1:0")?;
    let sink_addr = sink.local_addr()?;

    // --- Wiring -----------------------------------------------------------
    let (ready_tx, ready_rx) = std_mpsc::channel();
    let (turn_tx, turn_rx) = std_mpsc::channel::<TurnJob>();
    let turn_rx = Arc::new(Mutex::new(turn_rx));
    let (report_tx, report_rx) = std_mpsc::channel();
    let mut links: Vec<Vec<BatcherLink>> = config.batcher_cpus.iter().map(|_| Vec::new()).collect();
    let mut shard_io = Vec::new();
    let mut ingress_txs = Vec::new();
    let mut control_txs = Vec::new();
    for index in 0..shards {
        let (job_tx, job_rx) = rtrb::RingBuffer::<VadJob>::new(VAD_RING);
        let (result_tx, result_rx) = rtrb::RingBuffer::<VadResult>::new(VAD_RING);
        links[index % config.batcher_cpus.len()].push(BatcherLink {
            jobs: job_rx,
            results: result_tx,
            states: vec![[0.0; VAD_STATE]; config.sessions],
            carried: None,
        });
        let (ingress_tx, ingress_rx) = mpsc::channel::<Envelope>(65_536);
        let (control_tx, control_rx) = mpsc::unbounded_channel::<Control>();
        ingress_txs.push(ingress_tx);
        control_txs.push(control_tx);
        shard_io.push(ShardIo {
            ingress: ingress_rx,
            control: control_rx,
            vad_tx: job_tx,
            vad_rx: result_rx,
            turn_tx: turn_tx.clone(),
            sink: sink_addr,
            reports: report_tx.clone(),
            ready: ready_tx.clone(),
        });
    }
    drop(turn_tx);
    drop(report_tx);

    // --- Inference threads first, so that their memory is in the baseline --
    let mut batchers = Vec::new();
    for (&cpu, links) in config.batcher_cpus.iter().zip(links) {
        let gauge = Arc::new(Gauge::default());
        let worker = Worker {
            gauge: Arc::clone(&gauge),
            stop: Arc::clone(&stop),
            epoch: Arc::clone(&epoch),
            ready: ready_tx.clone(),
        };
        let config = Arc::clone(&config);
        let handle = std::thread::Builder::new()
            .name(format!("batcher-{cpu}"))
            .spawn(move || batcher_thread(cpu, config, links, worker))?;
        batchers.push((cpu, gauge, handle));
    }
    let mut turns = Vec::new();
    for &cpu in &config.turn_cpus {
        let gauge = Arc::new(Gauge::default());
        let worker = Worker {
            gauge: Arc::clone(&gauge),
            stop: Arc::clone(&stop),
            epoch: Arc::clone(&epoch),
            ready: ready_tx.clone(),
        };
        let (config, jobs, controls) = (
            Arc::clone(&config),
            Arc::clone(&turn_rx),
            control_txs.clone(),
        );
        let handle = std::thread::Builder::new()
            .name(format!("turn-{cpu}"))
            .spawn(move || turn_thread(cpu, config, jobs, controls, worker))?;
        turns.push((cpu, gauge, handle));
    }
    for _ in 0..batchers.len() + turns.len() {
        ready_rx
            .recv_timeout(Duration::from_secs(120))
            .map_err(|_| "an inference thread did not start")?;
    }
    let rss_base = rss_bytes();

    // --- Shards, then load ---------------------------------------------------
    let mut shard_handles = Vec::new();
    for (index, io) in shard_io.into_iter().enumerate() {
        let cpu = config.shard_cpus[index];
        let (config, assets) = (Arc::clone(&config), Arc::clone(&assets));
        shard_handles.push(
            std::thread::Builder::new()
                .name(format!("shard-{cpu}"))
                .spawn(move || shard_thread(index, cpu, config, assets, io))?,
        );
    }
    for _ in 0..shards {
        ready_rx
            .recv_timeout(Duration::from_secs(120))
            .map_err(|_| "a shard did not start")?;
    }
    let go = Instant::now() + Duration::from_millis(50);
    let mut generators = Vec::new();
    for (index, ingress) in ingress_txs.into_iter().enumerate() {
        let cpu = config.generator_cpus[index % config.generator_cpus.len()];
        let (sessions, stop) = (config.sessions, Arc::clone(&stop));
        generators.push(std::thread::spawn(move || {
            generator_thread(index, cpu, sessions, ingress, go, stop)
        }));
    }

    // --- Warm up, measure ------------------------------------------------------
    let per_second = config.churn * config.sessions as f64 / 60.0;
    let mut owed = 0.0_f64;
    let second = |owed: &mut f64| {
        std::thread::sleep(Duration::from_secs(1));
        *owed += per_second;
        let whole = owed.floor();
        if whole >= 1.0 {
            *owed -= whole;
            for control in &control_txs {
                let _ = control.send(Control::Replace(whole as u32));
            }
        }
    };
    for _ in 0..config.warmup.as_secs() {
        second(&mut owed);
    }
    let rss_warm = rss_bytes();
    epoch.fetch_add(1, Ordering::Relaxed);
    for control in &control_txs {
        let _ = control.send(Control::Begin);
    }
    // Workers publish their CPU time every 100 ms; let one period pass.
    std::thread::sleep(Duration::from_millis(150));
    let measure_start = Instant::now();
    let batcher_before: Vec<[u64; 4]> = batchers.iter().map(|(_, g, _)| g.read()).collect();
    let turn_before: Vec<[u64; 4]> = turns.iter().map(|(_, g, _)| g.read()).collect();
    let (mut rss_min, mut rss_max) = (u64::MAX, 0);
    for _ in 0..config.measure.as_secs() {
        second(&mut owed);
        let rss = rss_bytes();
        rss_min = rss_min.min(rss);
        rss_max = rss_max.max(rss);
    }
    std::thread::sleep(Duration::from_millis(150));
    let measured = measure_start.elapsed();
    let batcher_after: Vec<[u64; 4]> = batchers.iter().map(|(_, g, _)| g.read()).collect();
    let turn_after: Vec<[u64; 4]> = turns.iter().map(|(_, g, _)| g.read()).collect();
    for control in &control_txs {
        let _ = control.send(Control::End);
    }
    let mut reports: Vec<ShardReport> = Vec::new();
    for _ in 0..shards {
        reports.push(
            report_rx
                .recv_timeout(Duration::from_secs(30))
                .map_err(|_| "a shard did not report")?,
        );
    }
    reports.sort_by_key(|report| report.shard);
    let rss_end = rss_bytes();

    // --- Stop -------------------------------------------------------------------
    stop.store(true, Ordering::Relaxed);
    for control in &control_txs {
        let _ = control.send(Control::Shutdown);
    }
    let mut ingress_overflow = 0;
    for generator in generators {
        ingress_overflow += generator.join().unwrap_or(0);
    }
    let mut calibrations = Vec::new();
    for handle in shard_handles {
        let (before, after) = handle.join().map_err(|_| "a shard panicked")??;
        calibrations.push((before.as_secs_f64() * 1e3, after.as_secs_f64() * 1e3));
    }
    let mut vad_inference = Histogram::new();
    for (_, _, handle) in batchers.drain(..).collect::<Vec<_>>() {
        vad_inference.merge(&handle.join().map_err(|_| "a batcher panicked")??);
    }
    let (mut feature_time, mut turn_inference) = (Histogram::new(), Histogram::new());
    for (_, _, handle) in turns.drain(..).collect::<Vec<_>>() {
        let (features, inference) = handle.join().map_err(|_| "a turn thread panicked")??;
        feature_time.merge(&features);
        turn_inference.merge(&inference);
    }

    // --- Report ------------------------------------------------------------------
    let mut all = Stats::default();
    for report in &reports {
        all.merge(&report.stats);
    }
    let shard_busy: Vec<f64> = reports
        .iter()
        .map(|report| percent(report.run_ns, report.wall))
        .collect();
    let shard_wait: Vec<f64> = reports
        .iter()
        .map(|report| percent(report.wait_ns, report.wall))
        .collect();
    let delta = |before: &[[u64; 4]], after: &[[u64; 4]], field: usize| -> Vec<u64> {
        before
            .iter()
            .zip(after)
            .map(|(b, a)| a[field] - b[field])
            .collect()
    };
    let batcher_run = delta(&batcher_before, &batcher_after, 0);
    let batcher_busy: Vec<f64> = batcher_run
        .iter()
        .map(|&ns| percent(ns, measured))
        .collect();
    let batches: u64 = delta(&batcher_before, &batcher_after, 2).iter().sum();
    let batch_items: u64 = delta(&batcher_before, &batcher_after, 3).iter().sum();
    let turn_run = delta(&turn_before, &turn_after, 0);
    let turn_busy: Vec<f64> = turn_run.iter().map(|&ns| percent(ns, measured)).collect();
    let turn_jobs: u64 = delta(&turn_before, &turn_after, 2).iter().sum();

    let shard_ns: u64 = reports.iter().map(|report| report.run_ns).sum();
    let shard_wall = reports.first().map_or(measured, |report| report.wall);
    let per_session = |ns: u64, wall: Duration| percent(ns, wall) / total_sessions as f64;
    let cost_shard = per_session(shard_ns, shard_wall);
    let cost_vad = per_session(batcher_run.iter().sum(), measured);
    let cost_turn = per_session(turn_run.iter().sum(), measured);
    let cost_total = cost_shard + cost_vad + cost_turn;
    let mean_batch = batch_items as f64 / batches.max(1) as f64;
    let tick = all.tick_late.summary();
    let ingress = all.ingress_delay.summary();
    let memory_per_session = rss_warm.saturating_sub(rss_base) as f64 / total_sessions as f64;

    println!(
        "{}: {} sessions on {shards} shard(s), {} backend, {} allocator, {:?} batching, \
         complexity {}, churn {:.0}%/min",
        config.label,
        total_sessions,
        config.backend.name(),
        density_spike::alloc::name(),
        config.policy,
        config.complexity,
        config.churn * 100.0,
    );
    println!(
        "  shard busy {:.1?}%  (waiting for a CPU {:.2?}%)",
        shard_busy, shard_wait
    );
    println!(
        "  batcher busy {batcher_busy:.1?}%  mean batch {mean_batch:.1}  \
         inference p50 {:.0} us p99 {:.0} us",
        vad_inference.quantile(0.5) as f64 / 1e3,
        vad_inference.quantile(0.99) as f64 / 1e3
    );
    println!(
        "  turn busy {turn_busy:.1?}%  {turn_jobs} inferences  features p50 {:.1} ms  \
         inference p50 {:.1} ms  request-to-result p50 {:.0} ms p99 {:.0} ms",
        feature_time.quantile(0.5) as f64 / 1e6,
        turn_inference.quantile(0.5) as f64 / 1e6,
        all.turn_latency.quantile(0.5) as f64 / 1e6,
        all.turn_latency.quantile(0.99) as f64 / 1e6,
    );
    println!(
        "  per session: shard {cost_shard:.3}% + voice activity {cost_vad:.3}% + end of turn \
         {cost_turn:.3}% = {cost_total:.3}% of a core  ->  {:.0} sessions per core at 100%, \
         {:.0} at 75%",
        100.0 / cost_total,
        75.0 / cost_total
    );
    println!(
        "  tick lateness p50 {:.2} ms p99 {:.2} ms max {:.2} ms;  ingress delay p50 {:.2} ms \
         p99 {:.2} ms max {:.2} ms",
        tick.p50 as f64 / 1e6,
        tick.p99 as f64 / 1e6,
        tick.max as f64 / 1e6,
        ingress.p50 as f64 / 1e6,
        ingress.p99 as f64 / 1e6,
        ingress.max as f64 / 1e6
    );
    println!(
        "  decode p50 {:.1} us p99 {:.1} us;  encode p50 {:.1} us p99 {:.1} us;  provider \
         message p50 {:.1} us;  handlers over 100 us: {}",
        all.decode.quantile(0.5) as f64 / 1e3,
        all.decode.quantile(0.99) as f64 / 1e3,
        all.encode.quantile(0.5) as f64 / 1e3,
        all.encode.quantile(0.99) as f64 / 1e3,
        all.provider.quantile(0.5) as f64 / 1e3,
        all.slow_handlers
    );
    println!(
        "  voice activity: {} frames pushed, {} dropped, {} gated, {} results ({:.0}% speech); \
         push-to-result p50 {:.2} ms p99 {:.2} ms; delivered p50 {:.2} ms p99 {:.2} ms",
        all.vad_pushed,
        all.vad_dropped,
        all.vad_gated,
        all.vad_results,
        100.0 * all.vad_speech as f64 / all.vad_results.max(1) as f64,
        all.vad_compute.quantile(0.5) as f64 / 1e6,
        all.vad_compute.quantile(0.99) as f64 / 1e6,
        all.vad_delivered.quantile(0.5) as f64 / 1e6,
        all.vad_delivered.quantile(0.99) as f64 / 1e6
    );
    println!(
        "  memory: base {:.0} MB, with sessions {:.0} MB ({:.0} KB per session), during the \
         run {:.0}-{:.0} MB, at the end {:.0} MB",
        rss_base as f64 / 1e6,
        rss_warm as f64 / 1e6,
        memory_per_session / 1e3,
        rss_min as f64 / 1e6,
        rss_max as f64 / 1e6,
        rss_end as f64 / 1e6
    );
    println!(
        "  session start: first sessions p50 {:.0} us p99 {:.0} us; replacements ({}) p50 \
         {:.0} us p99 {:.0} us max {:.0} us",
        all.start_initial.quantile(0.5) as f64 / 1e3,
        all.start_initial.quantile(0.99) as f64 / 1e3,
        all.start_churn.count(),
        all.start_churn.quantile(0.5) as f64 / 1e3,
        all.start_churn.quantile(0.99) as f64 / 1e3,
        all.start_churn.quantile(1.0) as f64 / 1e3
    );
    println!(
        "  overflow: mailbox {}, ingress {ingress_overflow}, pacing resyncs {};  calibration \
         (before, after) ms {calibrations:.1?}",
        all.mailbox_overflow, all.pace_resyncs
    );

    if let Some(out) = options.value("--out") {
        let list = |values: &[f64]| {
            values
                .iter()
                .map(|v| format!("{v:.3}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        let mut json = String::new();
        write!(
            json,
            "{{\"label\":\"{}\",\"backend\":\"{}\",\"allocator\":\"{}\",\"policy\":\"{:?}\",\
             \"complexity\":{},\"churn_per_minute\":{},\"energy_gate\":{},\"shards\":{shards},\
             \"sessions\":{total_sessions},\"shard_cpus\":{:?},\"batcher_cpus\":{:?},\
             \"turn_cpus\":{:?},\"measured_s\":{:.1},",
            config.label,
            config.backend.name(),
            density_spike::alloc::name(),
            config.policy,
            config.complexity,
            config.churn,
            config.energy_gate,
            config.shard_cpus,
            config.batcher_cpus,
            config.turn_cpus,
            measured.as_secs_f64(),
        )?;
        write!(
            json,
            "\"shard_busy_pct\":[{}],\"shard_runqueue_wait_pct\":[{}],\"batcher_busy_pct\":[{}],\
             \"turn_busy_pct\":[{}],\"cost_pct_of_core_per_session\":{{\"shard\":{cost_shard:.4},\
             \"vad\":{cost_vad:.4},\"turn\":{cost_turn:.4},\"total\":{cost_total:.4}}},\
             \"mean_batch\":{mean_batch:.2},\"turn_inferences\":{turn_jobs},",
            list(&shard_busy),
            list(&shard_wait),
            list(&batcher_busy),
            list(&turn_busy),
        )?;
        write!(
            json,
            "\"tick_late_ms\":{},\"ingress_delay_ms\":{},\"decode_us\":{},\"encode_us\":{},\
             \"provider_us\":{},\"vad_inference_us\":{},\"vad_push_to_result_ms\":{},\
             \"vad_delivered_ms\":{},\"turn_features_ms\":{},\"turn_inference_ms\":{},\
             \"turn_request_to_result_ms\":{},\"session_start_initial_us\":{},\
             \"session_start_churn_us\":{},",
            tick.json(1e6),
            ingress.json(1e6),
            all.decode.summary().json(1e3),
            all.encode.summary().json(1e3),
            all.provider.summary().json(1e3),
            vad_inference.summary().json(1e3),
            all.vad_compute.summary().json(1e6),
            all.vad_delivered.summary().json(1e6),
            feature_time.summary().json(1e6),
            turn_inference.summary().json(1e6),
            all.turn_latency.summary().json(1e6),
            all.start_initial.summary().json(1e3),
            all.start_churn.summary().json(1e3),
        )?;
        write!(
            json,
            "\"vad_pushed\":{},\"vad_dropped\":{},\"vad_gated\":{},\"vad_results\":{},\
             \"vad_speech\":{},\"turn_requests\":{},\"turn_results\":{},\"slow_handlers\":{},\
             \"mailbox_overflow\":{},\"ingress_overflow\":{ingress_overflow},\
             \"pace_resyncs\":{},\"encoded_bytes\":{},\"rss_bytes\":{{\"base\":{rss_base},\
             \"warm\":{rss_warm},\"min\":{rss_min},\"max\":{rss_max},\"end\":{rss_end}}},\
             \"calibration_ms\":{:?}}}",
            all.vad_pushed,
            all.vad_dropped,
            all.vad_gated,
            all.vad_results,
            all.vad_speech,
            all.turn_requests,
            all.turn_results,
            all.slow_handlers,
            all.mailbox_overflow,
            all.pace_resyncs,
            all.encoded_bytes,
            calibrations
                .iter()
                .map(|(before, after)| [*before, *after])
                .collect::<Vec<_>>(),
        )?;
        json.push('\n');
        use std::io::Write as _;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(out)?
            .write_all(json.as_bytes())?;
    }
    drop(sink);
    Ok(())
}
