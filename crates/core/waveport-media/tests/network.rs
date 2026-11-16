//! The inbound path against networks made up from a seed.
//!
//! Each run sends one stream through a network that delays, loses, repeats
//! and overtakes packets as its seed says, and checks what the path hands
//! on against rules that hold whatever the network does. A failure names
//! its seed; `WAVEPORT_SIM_SEED=<seed>` runs that network alone, and
//! `WAVEPORT_SIM_SEEDS=<count>` runs more of them.

use waveport_domain::time::{MediaTime, MonoTime};
use waveport_media::codec::Codec;
use waveport_media::ingress::{Heard, Ingress, IngressConfig, IngressStats, Packet, Source, Taken};
use waveport_media::rate::Rate;
use waveport_sim::{SeedPlan, SimContext, run};

/// Packets in a stream.
const PACKETS: u64 = 150;

/// Samples in a packet: 20 ms at 16 kHz.
const SAMPLES: usize = 320;

/// How long a packet is on the media clock.
const TICKS: u64 = 960;

/// A piece of audio the path handed on.
#[derive(Clone, Copy, Debug)]
struct Piece {
    at: MediaTime,
    end: MediaTime,
    source: Source,
    begins: bool,
    /// Which packet it is, if it is one.
    packet: Option<u64>,
}

/// The audio of packet `n`: every sample says which packet it is from.
fn payload(n: u64) -> Vec<u8> {
    let sample = i16::try_from(n + 1).unwrap_or(i16::MAX);
    sample.to_le_bytes().repeat(SAMPLES)
}

/// When each copy of each packet arrives, in milliseconds, in order of
/// arrival.
fn arrivals(context: &mut SimContext) -> Vec<(u64, u64)> {
    let rng = &mut context.rng;
    // What kind of network this is.
    let jitter = *rng.choose(&[1, 1, 30, 80, 300]).unwrap_or(&1);
    let loss = *rng.choose(&[0, 0, 5, 30]).unwrap_or(&0);
    let repeats = *rng.choose(&[0, 0, 10]).unwrap_or(&0);
    // Some networks go away for a while.
    let outage = rng.chance(1, 3).then(|| {
        let from = rng.range(5..PACKETS - 5);
        from..from + rng.range(2..130)
    });

    let mut arrivals = Vec::new();
    for n in 0..PACKETS {
        if outage.as_ref().is_some_and(|gone| gone.contains(&n)) || rng.chance(loss, 100) {
            continue;
        }
        let sent = 20 * n;
        arrivals.push((sent + 5 + rng.range(0..jitter), n));
        if rng.chance(repeats, 100) {
            arrivals.push((sent + 5 + rng.range(0..jitter + 40), n));
        }
    }
    // Some hold packets back for a while and deliver them all at once.
    if rng.chance(1, 4) {
        let from = rng.range(100..2_500);
        let until = from + rng.range(50..400);
        for (at, _) in &mut arrivals {
            if (from..until).contains(at) {
                *at = until;
            }
        }
    }
    arrivals.sort_unstable();
    arrivals
}

/// Sends one stream through one network, and returns what the path counted.
fn stream(context: &mut SimContext) -> Result<IngressStats, String> {
    let codec = Codec::Pcm(Rate::Hz16000);
    let mut ingress = Ingress::new(IngressConfig::new(codec)).map_err(|e| e.to_string())?;
    let mut pieces: Vec<Piece> = Vec::new();
    let mut sink = |heard: Heard<'_>| {
        let first = heard.pcm.first().copied().unwrap_or(0);
        let same = heard.pcm.iter().all(|&sample| sample == first);
        let packet = (heard.source == Source::Packet && same && first > 0)
            .then(|| u64::try_from(first).unwrap_or(0) - 1);
        pieces.push(Piece {
            at: heard.at,
            end: heard.at.after(Rate::Hz16000.length_of(heard.pcm.len())),
            source: heard.source,
            begins: heard.begins,
            packet,
        });
    };

    let mut deadline: Option<MonoTime> = None;
    let mut taken = Vec::new();
    let mut pushes = 0_u64;
    let arrivals = arrivals(context);
    for (index, &(at_ms, n)) in arrivals.iter().enumerate() {
        context.tick();
        let now = MonoTime::from_millis(at_ms);
        // What the path asked to be woken for comes first.
        while let Some(due) = deadline.filter(|due| *due <= now) {
            deadline = ingress.poll(due, &mut sink);
            if deadline.is_some_and(|next| next <= due) {
                return Err(format!("asked to be woken at {due:?}, which had come"));
            }
        }
        let bytes = payload(n);
        let packet = Packet {
            // Numbers and timestamps wrap round in the middle of the
            // stream.
            sequence: 65_470_u16.wrapping_add(u16::try_from(n).unwrap_or(0)),
            timestamp: 0xFFFF_0000_u32.wrapping_add(u32::try_from(n).unwrap_or(0) * 320),
            payload: &bytes,
        };
        pushes += 1;
        if ingress.push(now, packet) == Taken::Held {
            taken.push(n);
        }
        // Packets that arrive together are all taken before any is
        // handed on.
        if arrivals.get(index + 1).is_some_and(|next| next.0 == at_ms) {
            continue;
        }
        deadline = ingress.poll(now, &mut sink);
        if deadline.is_some_and(|next| next <= now) {
            return Err(format!("asked to be woken at {now:?}, which had come"));
        }
    }
    // Long after the last packet nothing is waited for.
    let end = MonoTime::from_millis(20 * PACKETS + 10_000);
    if let Some(next) = ingress.poll(end, &mut sink) {
        return Err(format!("still waiting, until {next:?}, at the end"));
    }

    check(&pieces, &taken)?;
    let stats = ingress.stats();
    let handed = pieces.iter().filter(|piece| piece.packet.is_some()).count() as u64;
    if stats.packets != handed {
        return Err(format!(
            "{} packets held, {handed} handed on",
            stats.packets
        ));
    }
    if stats.packets + stats.late + stats.twice + stats.refused != pushes {
        return Err(format!("{pushes} packets do not add up: {stats:?}"));
    }
    Ok(stats)
}

/// The rules.
fn check(pieces: &[Piece], taken: &[u64]) -> Result<(), String> {
    let mut end = MediaTime::ZERO;
    // The packet a stretch of unbroken audio began with, and where.
    let mut base: Option<(u64, MediaTime)> = None;
    let mut last: Option<u64> = None;
    let mut made_up = 0;
    for (index, piece) in pieces.iter().enumerate() {
        let what = format!("piece {index} ({piece:?})");
        // Audio is handed on in order and without holes.
        if !piece.begins && index > 0 && piece.at != end {
            return Err(format!(
                "{what} does not follow the audio before it, which ends at {end:?}"
            ));
        }
        if piece.begins {
            if piece.at < end {
                return Err(format!(
                    "{what} begins before the audio before it ends, at {end:?}"
                ));
            }
            if piece.source != Source::Packet {
                return Err(format!("{what} begins a stream with something made up"));
            }
            base = None;
        } else if index == 0 {
            return Err(format!("{what} is the first and does not begin the stream"));
        }
        end = piece.end;

        match piece.source {
            Source::MadeUp => {
                made_up += 1;
                if made_up > 3 {
                    return Err(format!("{what} is the fourth in a row to be made up"));
                }
            }
            Source::Silence => {}
            Source::Packet => {
                made_up = 0;
                let Some(n) = piece.packet else {
                    return Err(format!("{what} is not the audio of any packet"));
                };
                // Each packet once, and in order.
                if last.is_some_and(|last| n <= last) {
                    return Err(format!("{what} is packet {n}, after packet {last:?}"));
                }
                last = Some(n);
                // The stream keeps its length: a packet is as far after
                // the one its stretch began with as it was sent after it.
                match base {
                    None => base = Some((n, piece.at)),
                    Some((first, began)) => {
                        let expected = MediaTime::from_ticks(began.ticks() + (n - first) * TICKS);
                        if piece.at != expected {
                            return Err(format!(
                                "{what} is packet {n} and belongs at {expected:?}: its stretch began with packet {first} at {began:?}"
                            ));
                        }
                    }
                }
            }
        }
    }
    // Every packet that was taken is handed on.
    let handed: Vec<u64> = pieces.iter().filter_map(|piece| piece.packet).collect();
    if handed != taken_in_order(taken) {
        return Err(format!(
            "taken: {taken:?}\nhanded on: {handed:?}\nthese differ"
        ));
    }
    Ok(())
}

fn taken_in_order(taken: &[u64]) -> Vec<u64> {
    let mut taken = taken.to_vec();
    taken.sort_unstable();
    taken
}

#[test]
fn whatever_the_network_does_the_stream_comes_out_in_order() {
    let default = SeedPlan::Range {
        start: 0,
        count: 500,
    };
    let plan = SeedPlan::from_env(default).unwrap();
    let mut seen = IngressStats::default();
    let summary = run(plan, |context| {
        let stats = stream(context)?;
        seen.packets += stats.packets;
        seen.late += stats.late;
        seen.twice += stats.twice;
        seen.refused += stats.refused;
        seen.made_up += stats.made_up;
        seen.skipped += stats.skipped;
        seen.breaks += stats.breaks;
        Ok::<(), String>(())
    })
    .unwrap();
    assert!(summary.seeds > 0);
    if plan != default {
        return;
    }
    // The networks do what they are there to do.
    for (what, count, least) in [
        ("packets handed on", seen.packets, 40_000),
        ("late", seen.late, 2_000),
        ("twice", seen.twice, 200),
        ("no room", seen.refused, 100),
        ("made up", seen.made_up, 2_000),
        ("skipped", seen.skipped, 2_000),
        ("breaks", seen.breaks, 20),
    ] {
        assert!(count >= least, "{what}: {count} in {seen:?}");
    }
}
