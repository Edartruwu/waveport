//! The transport, with a person who follows a script at the far end.
//!
//! The clock is the test's: a second of a call takes no time, and the
//! same call is the same every time.

#![expect(
    clippy::panic,
    clippy::unwrap_used,
    reason = "a helper of these tests that cannot go on fails the test that called it"
)]

use core::time::Duration;

use waveport_adapter_kit::wav::Wav;
use waveport_domain::ids::{Epoch, GenId, OpTag, ParticipantId};
use waveport_domain::time::MediaDur;
use waveport_ports::transport::{FrameOut, Transport, TransportControl, TransportEvent};
use waveport_testkit::drive::next_transport;
use waveport_transport_local_audio::{Choice, LocalAudio, Options, RATE, Recording, Step, devices};

const WHO: ParticipantId = ParticipantId(0);

fn tag(generation: u32) -> OpTag {
    OpTag {
        generation: GenId(generation),
        epoch: Epoch(1),
    }
}

/// A steady sound that is nowhere silent.
fn sound(samples: usize) -> Vec<i16> {
    (0..samples)
        .map(|n| if n % 96 < 48 { 6_000 } else { -6_000 })
        .collect()
}

fn bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

fn samples(payload: &[u8]) -> Vec<i16> {
    payload
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair))
        .collect()
}

/// What a session saw of a transport.
#[derive(Debug, Default)]
struct Seen {
    /// What the microphone heard, frame after frame.
    heard: Vec<i16>,
    /// What the loudspeaker said it had played, in order.
    played: Vec<(OpTag, u32)>,
    joined: bool,
    left: bool,
    closed: bool,
}

impl Seen {
    fn take(&mut self, event: Option<TransportEvent>) {
        match event {
            Some(TransportEvent::Joined { who }) => {
                assert_eq!(who, WHO);
                self.joined = true;
            }
            Some(TransportEvent::Left { who }) => {
                assert_eq!(who, WHO);
                self.left = true;
            }
            Some(TransportEvent::Audio { who, payload, .. }) => {
                assert_eq!(who, WHO);
                assert_eq!(payload.len(), 1_920);
                self.heard.extend(samples(&payload));
            }
            Some(TransportEvent::Played { tag, played }) => {
                self.played.push((tag, played.ticks()));
            }
            Some(other) => panic!("a sound card does not do this: {other:?}"),
            None => self.closed = true,
        }
    }

    /// The last the loudspeaker said of `tag`.
    fn last(&self, tag: OpTag) -> Option<u32> {
        self.played
            .iter()
            .rev()
            .find(|(of, _)| *of == tag)
            .map(|(_, played)| *played)
    }
}

/// Takes what the transport has to say for `time`.
async fn listen(transport: &mut LocalAudio, seen: &mut Seen, time: Duration) {
    let until = tokio::time::Instant::now() + time;
    while !seen.closed {
        match tokio::time::timeout_at(until, next_transport(transport)).await {
            Ok(event) => seen.take(event),
            Err(_) => return,
        }
    }
}

/// Sends `audio` as a response would be sent: a frame every 20 ms, the
/// first two together. Returns how many samples were taken.
async fn speak(
    transport: &mut LocalAudio,
    seen: &mut Seen,
    tag: OpTag,
    audio: &[i16],
    finish: bool,
) -> usize {
    let frames: Vec<_> = audio.chunks(960).collect();
    let mut at = 0;
    for (n, frame) in frames.iter().enumerate() {
        let out = FrameOut {
            tag,
            at: MediaDur::from_ticks(u32::try_from(at).unwrap()),
            length: MediaDur::from_ticks(u32::try_from(frame.len()).unwrap()),
            payload: &bytes(frame),
            last: finish && n + 1 == frames.len(),
        };
        transport.try_send(out).unwrap();
        at += frame.len();
        if n > 0 {
            listen(transport, seen, Duration::from_millis(20)).await;
        }
    }
    at
}

fn scripted(steps: Vec<Step>) -> (LocalAudio, Recording, Seen) {
    let (transport, recording) = LocalAudio::scripted(steps, Options::default());
    (transport, recording, Seen::default())
}

#[tokio::test(start_paused = true)]
async fn the_person_is_there_says_their_piece_and_hangs_up() {
    let speech = sound(4_800);
    let (mut transport, _, mut seen) = scripted(vec![
        Step::Silence(Duration::from_millis(100)),
        Step::Speech(speech.clone()),
        Step::Silence(Duration::from_millis(60)),
    ]);
    assert_eq!(
        next_transport(&mut transport).await,
        Some(TransportEvent::Joined { who: WHO })
    );
    let mut numbers = Vec::new();
    loop {
        match next_transport(&mut transport).await {
            Some(TransportEvent::Audio {
                sequence,
                timestamp,
                payload,
                ..
            }) => {
                numbers.push((sequence, timestamp));
                seen.heard.extend(samples(&payload));
            }
            other => {
                seen.take(other);
                break;
            }
        }
    }
    // 260 ms, in frames of 20 ms that follow one another.
    assert_eq!(numbers.len(), 13);
    for (n, (sequence, timestamp)) in numbers.iter().enumerate() {
        assert_eq!(usize::from(*sequence), n);
        assert_eq!(*timestamp as usize, n * 960);
    }
    assert_eq!(seen.heard[..4_800], [0; 4_800]);
    assert_eq!(seen.heard[4_800..9_600], speech[..]);
    assert_eq!(seen.heard[9_600..], [0; 2_880]);
    // Gone, and then nothing more for ever.
    assert!(seen.left && !seen.closed);
    assert_eq!(next_transport(&mut transport).await, None);
    assert_eq!(next_transport(&mut transport).await, None);
    assert_eq!(tokio::time::Instant::now().elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn a_response_is_played_and_said_to_be_as_it_goes() {
    let (mut transport, recording, mut seen) =
        scripted(vec![Step::Silence(Duration::from_secs(5))]);
    let caps = transport.caps();
    assert!(caps.remote_buffer.clearable && caps.remote_buffer.reports_played);
    assert_eq!(caps.outbound.clock(), RATE);
    listen(&mut transport, &mut seen, Duration::from_millis(100)).await;
    assert!(seen.joined);

    // Half a second and a bit: the last frame is not a whole one.
    let audio = sound(24_500);
    let sent = speak(&mut transport, &mut seen, tag(1), &audio, true).await;
    assert_eq!(sent, 24_500);
    listen(&mut transport, &mut seen, Duration::from_millis(200)).await;

    // Said often, never backwards, and at the end exactly all of it.
    let said: Vec<u32> = seen.played.iter().map(|(_, played)| *played).collect();
    assert!(said.len() >= 10, "{said:?}");
    assert!(said.is_sorted(), "{said:?}");
    assert!(said.windows(2).all(|pair| pair[1] - pair[0] <= 2_880));
    assert_eq!(seen.last(tag(1)), Some(24_500));
    assert!(seen.played.iter().all(|(of, _)| *of == tag(1)));

    // It was played whole and without a break, and nothing else was.
    let played = recording.samples();
    let from = played.iter().position(|sample| *sample != 0).unwrap();
    assert_eq!(played[from..from + 24_500], audio[..]);
    assert!(played[from + 24_500..].iter().all(|sample| *sample == 0));
    // It began as soon as the device asked, 10 ms at most after it came.
    assert!((4_800..=5_280).contains(&from), "{from}");
}

#[tokio::test(start_paused = true)]
async fn a_response_that_is_discarded_falls_silent_and_says_how_far_it_got() {
    let (mut transport, recording, mut seen) =
        scripted(vec![Step::Silence(Duration::from_secs(5))]);
    listen(&mut transport, &mut seen, Duration::from_millis(40)).await;

    // A second is sent ahead, as a far end that buffers would be sent
    // it, and a fifth of a second later the agent is spoken over.
    let audio = sound(48_000);
    for (n, frame) in audio.chunks(960).take(40).enumerate() {
        let out = FrameOut {
            tag: tag(1),
            at: MediaDur::from_ticks(u32::try_from(n * 960).unwrap()),
            length: MediaDur::from_millis(20),
            payload: &bytes(frame),
            last: false,
        };
        transport.try_send(out).unwrap();
    }
    listen(&mut transport, &mut seen, Duration::from_millis(200)).await;
    let before = seen.last(tag(1)).unwrap();
    transport
        .try_control(TransportControl::Clear { tag: tag(1) })
        .unwrap();
    // A frame that comes after that is not played.
    let late = FrameOut {
        tag: tag(1),
        at: MediaDur::from_millis(800),
        length: MediaDur::from_millis(20),
        payload: &bytes(&audio[..960]),
        last: true,
    };
    transport.try_send(late).unwrap();
    listen(&mut transport, &mut seen, Duration::from_millis(300)).await;

    // About 200 ms were played, and 5 ms more while it fell silent.
    let played = seen.last(tag(1)).unwrap();
    assert!(
        played >= before && played > 9_000 && played < 11_000,
        "{played}"
    );
    let heard = recording.samples();
    let from = heard.iter().position(|sample| *sample != 0).unwrap();
    let audible = heard.iter().filter(|sample| **sample != 0).count();
    assert!(audible <= played as usize && audible + 8 >= played as usize);
    // The last of it is quieter than what was sent, and then nothing.
    let end = from + played as usize;
    assert!(heard[end - 60..end].iter().all(|s| s.abs() < 1_600));
    assert!(heard[end..].iter().all(|sample| *sample == 0));

    // The next response is played from its start.
    let next = sound(9_600);
    speak(&mut transport, &mut seen, tag(2), &next, true).await;
    listen(&mut transport, &mut seen, Duration::from_millis(200)).await;
    assert_eq!(seen.last(tag(2)), Some(9_600));
    // Nothing more was said of the first once its end was known.
    let first_end = seen
        .played
        .iter()
        .rposition(|(of, _)| *of == tag(1))
        .unwrap();
    let second_start = seen
        .played
        .iter()
        .position(|(of, _)| *of == tag(2))
        .unwrap();
    assert!(first_end < second_start);
}

#[tokio::test(start_paused = true)]
async fn a_person_who_listens_waits_for_the_agent_to_have_finished() {
    let speech = sound(960);
    let (mut transport, _, mut seen) = scripted(vec![
        Step::Listen {
            quiet: Duration::from_millis(300),
            at_most: Duration::from_secs(60),
        },
        Step::Speech(speech),
    ]);
    // For a second nobody says anything: the person goes on waiting.
    listen(&mut transport, &mut seen, Duration::from_secs(1)).await;
    assert!(seen.heard.iter().all(|sample| *sample == 0));

    // The agent speaks for 400 ms. 300 ms after it has stopped, the
    // person speaks, and then hangs up.
    let audio = sound(19_200);
    speak(&mut transport, &mut seen, tag(1), &audio, true).await;
    let spoken_by = seen.heard.len();
    listen(&mut transport, &mut seen, Duration::from_secs(2)).await;
    let answer = seen.heard.iter().position(|sample| *sample != 0).unwrap();
    assert!(answer >= spoken_by, "the person spoke over the agent");
    // 1 s of waiting, 400 ms of the agent, 300 ms of quiet, give or take
    // the pieces a device works in.
    let at_ms = answer / 48;
    assert!((1_700..=1_760).contains(&at_ms), "{at_ms}");
    assert!(seen.left && seen.closed);
}

#[tokio::test(start_paused = true)]
async fn a_person_who_is_not_answered_stops_listening_in_the_end() {
    let (mut transport, _, mut seen) = scripted(vec![Step::Listen {
        quiet: Duration::from_millis(300),
        at_most: Duration::from_secs(2),
    }]);
    listen(&mut transport, &mut seen, Duration::from_secs(3)).await;
    assert!(seen.closed);
    assert_eq!(seen.heard.len(), 96_000);
}

#[tokio::test(start_paused = true)]
async fn a_deaf_microphone_does_not_hear_the_agent_or_anyone_over_it() {
    let options = Options {
        half_duplex: true,
        ..Options::default()
    };
    let (mut transport, _) = LocalAudio::scripted(vec![Step::Speech(sound(96_000))], options);
    let mut seen = Seen::default();
    // The person talks for two seconds without a pause. For the first
    // half second they are heard.
    listen(&mut transport, &mut seen, Duration::from_millis(500)).await;
    assert!(seen.heard.iter().all(|sample| *sample != 0));
    let heard_before = seen.heard.len();

    // The agent speaks for 400 ms: nothing is heard meanwhile, nor for
    // 300 ms after.
    speak(&mut transport, &mut seen, tag(1), &sound(19_200), true).await;
    listen(&mut transport, &mut seen, Duration::from_secs(2)).await;
    assert_eq!(seen.heard.len(), 96_000);
    let deaf: Vec<usize> = seen
        .heard
        .iter()
        .enumerate()
        .filter(|(_, sample)| **sample == 0)
        .map(|(n, _)| n)
        .collect();
    let (first, last) = (deaf[0], deaf[deaf.len() - 1]);
    // Deaf in one stretch, from when the agent began.
    assert_eq!(deaf.len(), last - first + 1);
    assert!(
        first >= heard_before && first <= heard_before + 1_920,
        "{first}"
    );
    // 400 ms of the agent and 300 ms more, give or take a frame at
    // either end.
    let deaf_ms = deaf.len() / 48;
    assert!((640..=740).contains(&deaf_ms), "{deaf_ms}");
}

#[tokio::test(start_paused = true)]
async fn what_cannot_be_played_is_not_taken() {
    let (mut transport, _, mut seen) = scripted(vec![Step::Silence(Duration::from_secs(9))]);
    listen(&mut transport, &mut seen, Duration::from_millis(20)).await;
    let frame = bytes(&sound(960));
    let out = |at: u32, payload| FrameOut {
        tag: tag(1),
        at: MediaDur::from_millis(at),
        length: MediaDur::from_millis(20),
        payload,
        last: false,
    };
    // A second fits on the line. What comes on top of it does not, and
    // is refused, so that whoever sent it knows.
    for n in 0..50 {
        transport.try_send(out(n * 20, &frame)).unwrap();
    }
    assert!(transport.try_send(out(1_000, &frame)).is_err());
    // Half a sample, or nothing, is not audio: it is let go of.
    transport.try_send(out(1_000, &frame[..5])).unwrap();
    transport.try_send(out(1_000, &[])).unwrap();
    // A request about a response nobody has heard of is taken, and does
    // nothing.
    transport
        .try_control(TransportControl::Clear { tag: tag(9) })
        .unwrap();
    listen(&mut transport, &mut seen, Duration::from_millis(1_200)).await;
    assert_eq!(seen.last(tag(1)), Some(48_000));
}

#[test]
fn speech_in_a_file_is_brought_to_the_rate_of_the_line() {
    let file = Wav {
        hz: 16_000,
        samples: sound(1_600),
    };
    let Step::Speech(speech) = Step::speech(&file).unwrap() else {
        panic!("speech is speech");
    };
    // A tenth of a second, and the little the filter runs on by.
    assert!((4_800..4_900).contains(&speech.len()), "{}", speech.len());
    assert!(speech.iter().any(|sample| sample.abs() > 3_000));

    let odd = Wav {
        hz: 44_100,
        samples: vec![0; 10],
    };
    let error = Step::speech(&odd).unwrap_err();
    assert!(error.to_string().contains("44100 Hz"), "{error}");
}

/// Needs a sound system. Makes no sound and opens nothing.
#[test]
#[ignore = "needs a sound system"]
fn live_the_devices_of_this_machine_are_listed() {
    let found = devices().unwrap();
    for device in &found {
        println!("{device:?}");
    }
    assert!(
        found
            .iter()
            .filter(|device| device.default_microphone)
            .count()
            <= 1
    );
    assert!(
        found
            .iter()
            .filter(|device| device.default_loudspeaker)
            .count()
            <= 1
    );
    // Every device can be asked for by what it is listed as.
    let ids: std::collections::BTreeSet<_> = found.iter().map(|device| &device.id).collect();
    assert_eq!(ids.len(), found.len());
}

/// Needs a microphone and a loudspeaker, and is heard: a tone is played
/// for half a second while the microphone listens. Nothing the microphone
/// hears is kept.
#[tokio::test]
#[ignore = "needs a microphone and a loudspeaker, and makes a sound"]
async fn live_a_tone_is_played_while_the_microphone_listens() {
    let mut transport = LocalAudio::open(&Choice::default(), Options::default()).unwrap();
    let health = transport.health();
    let mut seen = Seen::default();
    listen(&mut transport, &mut seen, Duration::from_millis(300)).await;
    assert!(seen.joined);

    let tone: Vec<i16> = (0..24_000_u32)
        .map(|n| {
            let turn = f64::from(n) * 440.0 / 48_000.0 * core::f64::consts::TAU;
            #[expect(clippy::cast_possible_truncation, reason = "a sine is within range")]
            let sample = (turn.sin() * 6_000.0) as i16;
            sample
        })
        .collect();
    speak(&mut transport, &mut seen, tag(1), &tone, true).await;
    listen(&mut transport, &mut seen, Duration::from_millis(500)).await;

    println!(
        "heard {} ms, played {:?}, {} stumbles, {} lost",
        seen.heard.len() / 48,
        seen.last(tag(1)),
        health.stumbles(),
        health.lost()
    );
    assert_eq!(health.failure(), None);
    assert_eq!(seen.last(tag(1)), Some(24_000));
    // About 1.3 s went by, and the microphone was heard throughout.
    assert!(seen.heard.len() > 48_000, "{}", seen.heard.len());
}
