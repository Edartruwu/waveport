//! The caller's side of a call that goes over a WebSocket: the sound
//! devices of this machine, or a scripted caller, joined to an agent
//! that runs somewhere else.

use core::future::poll_fn;
use core::time::Duration;

use waveport_adapter_kit::stream::{Either, first};
use waveport_domain::ids::{Epoch, GenId, OpTag};
use waveport_domain::time::MediaDur;
use waveport_ports::error::PortError;
use waveport_ports::transport::{FrameOut, Transport, TransportControl, TransportEvent};
use waveport_transport_local_audio::{Choice, LocalAudio, Options, RATE};
use waveport_transport_websocket::{Client, Heard};

use crate::args::Client as Args;
use crate::call::{Outcome, tell_health};
use crate::{Context, Failure, script};

/// How long the agent's server has to answer a call.
const ANSWER_WITHIN: Duration = Duration::from_secs(10);

/// How long an agent's side that takes nothing more is still read for
/// what it had said.
const LAST_WORDS: Duration = Duration::from_secs(1);

/// How a call came to its end, seen from the caller's side.
enum Over {
    /// The agent's side said goodbye.
    Ended,
    /// The caller left.
    Left,
    /// The agent's side went away without a word, or with one that is
    /// not of the protocol.
    Gone(String),
}

/// What the devices of this machine know a response by. The agent's side
/// numbers its responses, and a number is all there is to one here.
const fn tag(run: u32) -> OpTag {
    OpTag {
        generation: GenId(run),
        epoch: Epoch(0),
    }
}

/// Calls the agent at `args.url` and stays on the call to its end.
pub(crate) async fn call(args: Args) -> Result<Outcome, Failure> {
    let (mut near, recording) = if let Some(file) = &args.script {
        let steps = script::read(file).map_err(Failure)?;
        let (near, recording) = LocalAudio::scripted(steps, Options::default());
        (near, Some(recording))
    } else {
        let choice = Choice {
            microphone: args.microphone.clone(),
            loudspeaker: args.loudspeaker.clone(),
        };
        let options = Options {
            half_duplex: args.half_duplex,
            ..Options::default()
        };
        let near = LocalAudio::open(&choice, options).with(|| {
            "the sound devices cannot be used (`waveport devices` lists them)".to_owned()
        })?;
        if args.half_duplex {
            println!("note: the microphone is deaf while the agent speaks");
        } else {
            println!("note: wear headphones, or the agent hears itself and stops speaking");
        }
        (near, None)
    };
    let health = near.health();
    let far = Client::connect(&args.url, RATE, ANSWER_WITHIN)
        .await
        .with(|| format!("no agent took the call at {}", args.url))?;
    println!("the call has begun");

    let over = tokio::select! {
        over = stay(&mut near, far) => over,
        // The connection goes with the command, and the agent's side
        // takes that for the caller leaving.
        _ = tokio::signal::ctrl_c() => Over::Left,
    };
    tell_health(&health);
    if let (Some(recording), Some(file)) = (recording, &args.record) {
        std::fs::write(file, recording.wav())
            .with(|| format!("the recording cannot be written to {}", file.display()))?;
        let millis = recording.samples().len() / 48;
        println!("recorded {millis} ms to {}", file.display());
    }
    match over {
        Over::Ended | Over::Left => Ok(Outcome::Ended),
        Over::Gone(why) => {
            eprintln!("waveport: the agent's side of the call is gone: {why}");
            Ok(Outcome::Broken)
        }
    }
}

/// The agent's side takes nothing more: sending to it failed with
/// `error`. It may have said how the call ended before it stopped
/// listening, so what it sent is read to its end. A call that was said
/// to be over is over, and not broken off.
async fn last_words(mut far: Client, error: &PortError) -> Over {
    let reading = async {
        loop {
            match far.next().await {
                Ok(Some(Heard::Line(line))) => println!("{line}"),
                Ok(Some(Heard::Bye(_))) => return true,
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return false,
            }
        }
    };
    match tokio::time::timeout(LAST_WORDS, reading).await {
        Ok(true) => Over::Ended,
        Ok(false) | Err(_) => Over::Gone(error.to_string()),
    }
}

/// Carries the call: what the microphone hears goes to the agent, what
/// the agent says goes to the loudspeaker, and the agent is told how
/// much of it has been played.
async fn stay(near: &mut LocalAudio, mut far: Client) -> Over {
    // The response whose audio arrives, and how many samples of it have.
    let mut open: Option<(u32, usize)> = None;
    loop {
        // What the agent's side says comes before what the microphone
        // hears: a caller who is told that the call is over has nothing
        // more to send.
        let here = poll_fn(|cx| near.poll_event(cx));
        let heard = match first(far.next(), here).await {
            Either::Right(Some(TransportEvent::Audio { payload, .. })) => {
                if let Err(error) = far.audio(payload).await {
                    return last_words(far, &error).await;
                }
                continue;
            }
            Either::Right(Some(TransportEvent::Played { tag, played })) => {
                let samples = u32::try_from(RATE.samples_in(played)).unwrap_or(u32::MAX);
                if let Err(error) = far.played(tag.generation.0, samples).await {
                    return last_words(far, &error).await;
                }
                continue;
            }
            Either::Right(Some(TransportEvent::Left { .. }) | None) => {
                far.bye().await;
                return Over::Left;
            }
            Either::Right(Some(_)) => continue,
            Either::Left(Ok(Some(heard))) => heard,
            Either::Left(Ok(None)) => {
                return Over::Gone("its connection closed with no goodbye".to_owned());
            }
            Either::Left(Err(error)) => return Over::Gone(error.to_string()),
        };
        match heard {
            Heard::Begins(run) => open = Some((run, 0)),
            Heard::Audio(samples) => {
                // Audio that no response was begun for belongs to none.
                if let Some((run, at)) = &mut open {
                    let length = samples.len() / 2;
                    let frame = FrameOut {
                        tag: tag(*run),
                        at: RATE.length_of(*at),
                        length: RATE.length_of(length),
                        payload: &samples,
                        last: false,
                    };
                    // A loudspeaker that is a second behind has no room.
                    // What does not fit is not played, and so is not said
                    // to have been.
                    let _ = near.try_send(frame);
                    *at += length;
                }
            }
            Heard::Ends(run) => {
                let at = match open {
                    Some((of, at)) if of == run => at,
                    _ => 0,
                };
                let end = FrameOut {
                    tag: tag(run),
                    at: RATE.length_of(at),
                    length: MediaDur::ZERO,
                    payload: &[],
                    last: true,
                };
                let _ = near.try_send(end);
                open = None;
            }
            Heard::Clear(run) => {
                let _ = near.try_control(TransportControl::Clear { tag: tag(run) });
                if open.is_some_and(|(of, _)| of <= run) {
                    open = None;
                }
            }
            Heard::Line(line) => println!("{line}"),
            Heard::Bye(_) => return Over::Ended,
        }
    }
}
