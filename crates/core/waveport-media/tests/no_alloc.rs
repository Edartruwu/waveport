//! The audio path must leave the heap alone.
//!
//! Every 20 ms each session takes a packet in and, while its agent speaks,
//! hands a frame out. Once a session is set up, none of that may call the
//! allocator: not decoding, not changing the rate, not waiting for a
//! packet that was overtaken, not making up one that was lost, not pacing,
//! not lowering the volume, not saying how far playback has got, and not
//! making the tone of a key.
//!
//! What is counted is the Rust allocator. libopus has its own memory,
//! which it takes when a codec is set up and not per frame.

use waveport_alloc_counter::{CountingAllocator, is_installed, measure};
use waveport_domain::event::Key;
use waveport_domain::ids::{Epoch, GenId, OpTag};
use waveport_domain::time::{MediaDur, MonoTime};
use waveport_media::codec::g711::Law;
use waveport_media::codec::opus::OpusSettings;
use waveport_media::codec::{Codec, Decoder, Encoder};
use waveport_media::dtmf::Dialer;
use waveport_media::ingress::{Ingress, IngressConfig, Packet};
use waveport_media::ledger::{Mark, Spread};
use waveport_media::playout::{Playout, PlayoutConfig};
use waveport_media::pool::Pool;
use waveport_media::rate::Rate;
use waveport_media::resample::Resampler;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator::system();

const CODECS: [Codec; 4] = [
    Codec::Opus,
    Codec::G711(Law::Mu),
    Codec::Pcm(Rate::Hz16000),
    Codec::Pcm(Rate::Hz48000),
];

/// Runs `work`, which may not call the allocator.
#[track_caller]
fn quiet<R>(what: &str, work: impl FnOnce() -> R) -> R {
    let (result, stats) = measure(work);
    assert_eq!(stats.calls(), 0, "{what} called the allocator: {stats:?}");
    result
}

/// A sawtooth: something for a codec to work on.
fn audio(samples: usize) -> Vec<i16> {
    (0..samples)
        .map(|n| i16::try_from(n % 200).unwrap_or(0) * 100 - 10_000)
        .collect()
}

/// `count` packets of 20 ms each.
#[expect(
    clippy::unwrap_used,
    reason = "a codec that cannot be set up fails the test"
)]
fn packets(codec: Codec, count: usize) -> Vec<Vec<u8>> {
    let rate = Rate::Hz48000;
    let mut encoder = Encoder::new(codec, rate, OpusSettings::default()).unwrap();
    audio(rate.frame() * count)
        .chunks(rate.frame())
        .map(|frame| {
            let mut packet = vec![0; codec.max_packet()];
            let len = encoder.encode(frame, &mut packet).unwrap();
            packet.truncate(len);
            packet
        })
        .collect()
}

#[test]
fn the_counter_is_counting() {
    assert!(is_installed());
    let (_, stats) = measure(|| vec![0_u8; 64]);
    assert!(stats.calls() > 0);
}

#[test]
fn a_packet_in_costs_no_allocation() {
    for codec in CODECS {
        let sent = packets(codec, 60);
        let mut ingress = Ingress::new(IngressConfig::new(codec)).unwrap();
        let step = codec.clock().frame();
        let mut heard = 0_usize;
        let mut queued = vec![0; 320];
        let mut window = vec![0; 16_000];

        let arrive = |ingress: &mut Ingress, ms: u64, n: usize, heard: &mut usize| {
            let packet = Packet {
                sequence: u16::try_from(n).unwrap(),
                timestamp: u32::try_from(n * step).unwrap(),
                payload: &sent[n],
            };
            let now = MonoTime::from_millis(ms);
            ingress.push(now, packet);
            ingress.poll(now, |piece| *heard += piece.pcm.len());
        };

        // The first packets set the stream up.
        for n in 0..5 {
            arrive(&mut ingress, 20 * u64::try_from(n).unwrap(), n, &mut heard);
        }
        quiet(&format!("{codec:?} in order"), || {
            for n in 5..20 {
                arrive(&mut ingress, 20 * u64::try_from(n).unwrap(), n, &mut heard);
                ingress.take_queued(&mut queued);
                ingress.window().latest(&mut window);
            }
        });
        quiet(&format!("{codec:?} with losses and overtaking"), || {
            // 20 is lost. 22 overtakes 21, which is then too late. 24
            // overtakes 23, which is waited for and comes. 30 comes after
            // a gap that is filled with silence.
            for (ms, n) in [
                (420, 21),
                (440, 22),
                (445, 21),
                (480, 24),
                (485, 23),
                (500, 25),
                (600, 30),
                (620, 31),
            ] {
                arrive(&mut ingress, ms, n, &mut heard);
            }
            // 34 comes twice while 32 is waited for, and the wait runs
            // out with 32 still missing.
            arrive(&mut ingress, 680, 34, &mut heard);
            arrive(&mut ingress, 681, 34, &mut heard);
            ingress.poll(MonoTime::from_millis(760), |piece| heard += piece.pcm.len());
            ingress.stats()
        });
        let stats = ingress.stats();
        assert!(
            stats.made_up > 0 && stats.late > 0 && stats.twice > 0,
            "{stats:?}"
        );
        assert!(heard > 320 * 30, "{codec:?}: {heard}");
    }
}

#[test]
fn a_frame_out_costs_no_allocation() {
    let rate = Rate::Hz24000;
    let tag = |generation| OpTag {
        generation: GenId(generation),
        epoch: Epoch(0),
    };
    let mut playout = Playout::new(PlayoutConfig::new(rate));
    let speech = audio(rate.frame() * 100);
    let mut frame = vec![0; rate.frame()];
    let mut encoders: Vec<Encoder> = CODECS
        .iter()
        .map(|&codec| Encoder::new(codec, rate, OpusSettings::default()).unwrap())
        .collect();
    let mut packet = vec![0; 8_000];

    quiet("two responses, one of them interrupted", || {
        let mut handed = 0;
        for (generation, start) in [(1, 0), (2, 5_000)] {
            let tag = tag(generation);
            let now = MonoTime::from_millis(start);
            playout.start(tag, now).unwrap();
            playout.push(tag, now, &speech).unwrap();
            for word in 1..=20 {
                let mark = Mark {
                    text: word * 6,
                    audio: MediaDur::from_millis(word * 100),
                    spread: Spread::AtOnce,
                };
                playout.mark(tag, mark).unwrap();
            }
            playout.finish(tag).unwrap();
            for step in 0..60_u64 {
                let now = MonoTime::from_millis(start + 20 * step);
                match step {
                    20 => playout.duck(tag),
                    30 => playout.unduck(tag),
                    50 if generation == 1 => playout.stop(tag, 10),
                    _ => {}
                }
                while playout.poll(now, &mut frame).is_some() {
                    handed += 1;
                    for encoder in &mut encoders {
                        encoder.encode(&frame, &mut packet).unwrap();
                    }
                }
                playout.position(now);
                let _ = playout.wake();
            }
        }
        assert!(handed > 100, "{handed}");
    });
}

#[test]
fn codecs_and_rate_changes_cost_no_allocation() {
    let mut out = vec![0; 8_000];
    for codec in CODECS {
        let sent = packets(codec, 12);
        let mut decoder = Decoder::new(codec, Rate::Hz16000).unwrap();
        let length = MediaDur::from_millis(20);
        quiet(&format!("{codec:?} decoding"), || {
            for packet in &sent[..8] {
                decoder.decode(packet, &mut out).unwrap();
            }
            decoder.conceal(length, &mut out).unwrap();
            decoder.recover(&sent[10], length, &mut out).unwrap();
            decoder.decode(&sent[10], &mut out).unwrap();
            decoder.length_of(&sent[11]).unwrap();
        });
    }
    for from in Rate::ALL {
        for to in Rate::ALL {
            let mut resampler = Resampler::new(from, to);
            let input = audio(from.frame() * 3);
            quiet(&format!("{from:?} to {to:?}"), || {
                resampler.process(&input, &mut out).unwrap();
                resampler.reset();
                resampler.process(&input[..7], &mut out).unwrap();
            });
        }
    }
}

#[test]
fn pressing_keys_costs_no_allocation() {
    for rate in Rate::ALL {
        let mut dialer = quiet("setting up a dialer", || Dialer::new(rate));
        let mut frame = audio(rate.frame());
        quiet("pressing keys", || {
            for key in Key::ALL {
                assert!(dialer.press(key));
            }
            // Every tone and every pause, a frame at a time, over audio
            // that is there already.
            while !dialer.is_idle() {
                assert_eq!(dialer.mix(&mut frame), frame.len());
            }
            dialer.clear();
        });
    }
}

#[test]
fn a_pool_that_is_large_enough_costs_no_allocation() {
    let mut pool = Pool::new(320, 4);
    let samples = audio(320);
    quiet("taking and giving back", || {
        for _ in 0..100 {
            let mut first = pool.take();
            let mut second = pool.take();
            first.fill(&samples);
            second.fill(first.as_slice());
            pool.give(first);
            pool.give(second);
        }
    });
    assert_eq!(pool.made(), 0);
}
