//! The sound devices of this machine.
//!
//! A device is opened at 48 kHz, in whatever form its samples take and
//! with however many channels it has: the microphone's channels are
//! averaged to one, and the one channel of the agent's voice is played on
//! every channel of the loudspeaker. A device that cannot run at 48 kHz
//! is refused, with the rates it does run at.

use core::fmt;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, Device, Error, ErrorKind, FromSample, Sample, SampleFormat, SizedSample, Stream,
    StreamConfig, SupportedBufferSize, SupportedStreamConfigRange,
};

use crate::line::{Alarm, DeviceEnd, HZ, Mic, Speaker};

/// How many frames a device is asked to take or give at a time: 10 ms.
/// A device that works in larger pieces than it has to makes everything
/// later: what the agent says, and the moment it stops when spoken over.
const PIECE: u32 = HZ / 100;

/// How many frames the callback of a loudspeaker works through at a time,
/// however many it is asked for.
const BLOCK: usize = 4_096;

/// A sound device of this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "four things that are each true or not of a device, and none follows from another"
)]
pub struct DeviceInfo {
    /// What it is asked for by: a name that stays the same from one run
    /// to the next.
    pub id: String,
    /// What it is called.
    pub name: String,
    /// Whether it can be a microphone.
    pub hears: bool,
    /// Whether it can be a loudspeaker.
    pub plays: bool,
    /// Whether it is the microphone used when none is asked for.
    pub default_microphone: bool,
    /// Whether it is the loudspeaker used when none is asked for.
    pub default_loudspeaker: bool,
}

/// Which devices to use. `None` is the one the system uses by default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Choice {
    /// The [`id`](DeviceInfo::id) of the microphone.
    pub microphone: Option<String>,
    /// The [`id`](DeviceInfo::id) of the loudspeaker.
    pub loudspeaker: Option<String>,
}

/// Why a sound device could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceError {
    /// The machine has no microphone.
    NoMicrophone,
    /// The machine has no loudspeaker.
    NoLoudspeaker,
    /// There is no device of this name.
    Unknown(String),
    /// The device does not run at 48 kHz.
    Rate {
        /// The device.
        device: String,
        /// The rates it runs at, each a lowest and a highest.
        offers: Vec<(u32, u32)>,
    },
    /// The device's samples are in a form that is not read or written
    /// here.
    Form {
        /// The device.
        device: String,
        /// The form, in the sound system's words.
        form: String,
    },
    /// The sound system refused.
    System(String),
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMicrophone => f.write_str("this machine has no microphone"),
            Self::NoLoudspeaker => f.write_str("this machine has no loudspeaker"),
            Self::Unknown(id) => write!(f, "there is no sound device called {id}"),
            Self::Rate { device, offers } => {
                write!(f, "{device} does not run at 48,000 Hz: it runs at")?;
                for (n, (low, high)) in offers.iter().enumerate() {
                    let comma = if n == 0 { "" } else { "," };
                    if low == high {
                        write!(f, "{comma} {low} Hz")?;
                    } else {
                        write!(f, "{comma} {low} to {high} Hz")?;
                    }
                }
                Ok(())
            }
            Self::Form { device, form } => {
                write!(
                    f,
                    "{device} has samples of a form that is not handled: {form}"
                )
            }
            Self::System(error) => write!(f, "the sound system refused: {error}"),
        }
    }
}

impl core::error::Error for DeviceError {}

impl From<Error> for DeviceError {
    fn from(error: Error) -> Self {
        Self::System(error.to_string())
    }
}

/// What a device is called, as well as that can be found out.
fn name(device: &Device) -> String {
    device.description().map_or_else(
        |_| device.to_string(),
        |description| description.name().to_owned(),
    )
}

/// The sound devices of this machine.
///
/// # Errors
///
/// The sound system could not be asked.
pub fn devices() -> Result<Vec<DeviceInfo>, DeviceError> {
    let host = cpal::default_host();
    let id = |device: Option<Device>| device.and_then(|device| device.id().ok());
    let microphone = id(host.default_input_device());
    let loudspeaker = id(host.default_output_device());
    let mut found = Vec::new();
    for device in host.devices()? {
        // A device that will not say what it is called cannot be asked
        // for by name, and is left out.
        let Ok(id) = device.id() else {
            continue;
        };
        found.push(DeviceInfo {
            name: name(&device),
            hears: device.supports_input(),
            plays: device.supports_output(),
            default_microphone: microphone.as_ref() == Some(&id),
            default_loudspeaker: loudspeaker.as_ref() == Some(&id),
            id: id.to_string(),
        });
    }
    Ok(found)
}

/// The streams of a microphone and a loudspeaker. They run for as long
/// as this is kept.
pub(crate) struct Card {
    _microphone: Stream,
    _loudspeaker: Stream,
}

impl fmt::Debug for Card {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Card").finish_non_exhaustive()
    }
}

/// The device called `id`, or the one `default` gives.
fn find(
    host: &cpal::Host,
    id: Option<&str>,
    default: impl FnOnce(&cpal::Host) -> Option<Device>,
    missing: DeviceError,
) -> Result<Device, DeviceError> {
    let Some(id) = id else {
        return default(host).ok_or(missing);
    };
    let unknown = || DeviceError::Unknown(id.to_owned());
    let parsed = id.parse().map_err(|_| unknown())?;
    host.device_by_id(&parsed).ok_or_else(unknown)
}

/// How well a form of samples suits: the one the line is in first, then
/// the ones that lose nothing of it.
const fn preference(format: SampleFormat) -> Option<u8> {
    Some(match format {
        SampleFormat::I16 => 0,
        SampleFormat::F32 => 1,
        SampleFormat::I32 => 2,
        SampleFormat::I24 => 3,
        SampleFormat::F64 => 4,
        SampleFormat::U16 => 5,
        SampleFormat::U32 => 6,
        SampleFormat::U24 => 7,
        SampleFormat::I64 => 8,
        SampleFormat::U64 => 9,
        SampleFormat::I8 => 10,
        SampleFormat::U8 => 11,
        _ => return None,
    })
}

/// The way of running `device` that suits best, of those it offers. With
/// `plain`, the device is left to work in pieces of whatever size it
/// likes.
fn choose(
    device: &Device,
    offered: impl Iterator<Item = SupportedStreamConfigRange>,
    plain: bool,
) -> Result<(StreamConfig, SampleFormat), DeviceError> {
    let offered: Vec<_> = offered.collect();
    let at_rate = |range: &&SupportedStreamConfigRange| {
        (range.min_sample_rate()..=range.max_sample_rate()).contains(&HZ)
    };
    let best = offered
        .iter()
        .filter(at_rate)
        .filter_map(|range| Some((preference(range.sample_format())?, range.channels(), range)))
        .min_by_key(|(preference, channels, _)| (*preference, *channels));
    let Some((_, channels, range)) = best else {
        let device = name(device);
        if let Some(range) = offered.iter().find(at_rate) {
            let form = range.sample_format().to_string();
            return Err(DeviceError::Form { device, form });
        }
        let mut offers: Vec<_> = offered
            .iter()
            .map(|range| (range.min_sample_rate(), range.max_sample_rate()))
            .collect();
        offers.sort_unstable();
        offers.dedup();
        return Err(DeviceError::Rate { device, offers });
    };
    let buffer_size = match range.buffer_size() {
        SupportedBufferSize::Range { min, max } if !plain && (*min..=*max).contains(&PIECE) => {
            BufferSize::Fixed(PIECE)
        }
        SupportedBufferSize::Range { .. } | SupportedBufferSize::Unknown => BufferSize::Default,
    };
    let config = StreamConfig {
        channels,
        sample_rate: HZ,
        buffer_size,
    };
    Ok((config, range.sample_format()))
}

/// What is done when a stream says that something went wrong. A device
/// that fell behind goes on, and so does one that was told it may not
/// run ahead of everything else or that its audio now goes by another
/// way. Anything else is the end of it.
fn on_error(alarm: Alarm, what: &'static str) -> impl FnMut(Error) + Send + 'static {
    move |error| match error.kind() {
        ErrorKind::Xrun => alarm.stumbled(),
        ErrorKind::RealtimeDenied | ErrorKind::DeviceChanged => {}
        _ => alarm.fail(format!("the {what} failed: {error}")),
    }
}

/// Starts a stream that takes what the microphone hears, in samples of
/// the form `T`.
fn hear<T>(
    device: &Device,
    config: StreamConfig,
    mut mic: Mic,
    alarm: Alarm,
) -> Result<Stream, Error>
where
    T: SizedSample,
    i16: FromSample<T>,
{
    let channels = usize::from(config.channels.max(1));
    let width = i32::from(config.channels.max(1));
    device.build_input_stream(
        config,
        move |heard: &[T], _: &cpal::InputCallbackInfo| {
            mic.push(heard.chunks_exact(channels).map(|frame| {
                let sum: i32 = frame
                    .iter()
                    .map(|sample| i32::from(i16::from_sample(*sample)))
                    .sum();
                i16::try_from(sum / width).unwrap_or(0)
            }));
        },
        on_error(alarm, "microphone"),
        None,
    )
}

/// Starts a stream that gives the loudspeaker what it is to play, in
/// samples of the form `T`.
fn play<T>(
    device: &Device,
    config: StreamConfig,
    mut speaker: Speaker,
    alarm: Alarm,
) -> Result<Stream, Error>
where
    T: SizedSample + FromSample<i16>,
{
    let channels = usize::from(config.channels.max(1));
    let mut voice = vec![0_i16; BLOCK];
    device.build_output_stream(
        config,
        move |wanted: &mut [T], _: &cpal::OutputCallbackInfo| {
            for block in wanted.chunks_mut(BLOCK * channels) {
                let frames = block.len() / channels;
                let voice = voice.get_mut(..frames).unwrap_or_default();
                speaker.fill(voice);
                for (frame, sample) in block.chunks_mut(channels).zip(voice.iter()) {
                    frame.fill(T::from_sample(*sample));
                }
            }
        },
        on_error(alarm, "loudspeaker"),
        None,
    )
}

/// Calls `$start::<T>` with the type of sample that `$format` names.
macro_rules! with_form {
    ($format:expr, $device:expr, $start:ident ( $($argument:expr),* )) => {
        match $format {
            cpal::SampleFormat::I8 => $start::<i8>($($argument),*).map_err(crate::card::DeviceError::from),
            cpal::SampleFormat::I16 => $start::<i16>($($argument),*).map_err(crate::card::DeviceError::from),
            cpal::SampleFormat::I24 => $start::<cpal::I24>($($argument),*).map_err(crate::card::DeviceError::from),
            cpal::SampleFormat::I32 => $start::<i32>($($argument),*).map_err(crate::card::DeviceError::from),
            cpal::SampleFormat::I64 => $start::<i64>($($argument),*).map_err(crate::card::DeviceError::from),
            cpal::SampleFormat::U8 => $start::<u8>($($argument),*).map_err(crate::card::DeviceError::from),
            cpal::SampleFormat::U16 => $start::<u16>($($argument),*).map_err(crate::card::DeviceError::from),
            cpal::SampleFormat::U24 => $start::<cpal::U24>($($argument),*).map_err(crate::card::DeviceError::from),
            cpal::SampleFormat::U32 => $start::<u32>($($argument),*).map_err(crate::card::DeviceError::from),
            cpal::SampleFormat::U64 => $start::<u64>($($argument),*).map_err(crate::card::DeviceError::from),
            cpal::SampleFormat::F32 => $start::<f32>($($argument),*).map_err(crate::card::DeviceError::from),
            cpal::SampleFormat::F64 => $start::<f64>($($argument),*).map_err(crate::card::DeviceError::from),
            other => Err(crate::card::DeviceError::Form {
                device: crate::card::name($device),
                form: other.to_string(),
            }),
        }
    };
}

/// Opens the microphone and the loudspeaker, and starts them. With
/// `plain`, they are left to work in pieces of whatever size they like:
/// a device may say that it can work in small ones and then refuse to.
pub(crate) fn open(choice: &Choice, end: DeviceEnd, plain: bool) -> Result<Card, DeviceError> {
    let host = cpal::default_host();
    let microphone = find(
        &host,
        choice.microphone.as_deref(),
        HostTrait::default_input_device,
        DeviceError::NoMicrophone,
    )?;
    let loudspeaker = find(
        &host,
        choice.loudspeaker.as_deref(),
        HostTrait::default_output_device,
        DeviceError::NoLoudspeaker,
    )?;
    let (hears, heard_as) = choose(&microphone, microphone.supported_input_configs()?, plain)?;
    let (plays, played_as) = choose(&loudspeaker, loudspeaker.supported_output_configs()?, plain)?;

    let DeviceEnd {
        mic,
        speaker,
        alarm,
    } = end;
    let hearing = with_form!(
        heard_as,
        &microphone,
        hear(&microphone, hears, mic, alarm.clone())
    )?;
    let playing = with_form!(
        played_as,
        &loudspeaker,
        play(&loudspeaker, plays, speaker, alarm)
    )?;
    // The loudspeaker first: a microphone that runs before there is
    // anything to answer with only fills the line.
    playing.play()?;
    hearing.play()?;
    Ok(Card {
        _microphone: hearing,
        _loudspeaker: playing,
    })
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::time::Instant;

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    use super::{DeviceError, choose, name, play};
    use crate::line::{Progress, line};

    #[test]
    fn a_refusal_says_what_the_device_does_offer() {
        let rate = DeviceError::Rate {
            device: "Old Card".to_owned(),
            offers: vec![(44_100, 44_100), (8_000, 32_000)],
        };
        assert_eq!(
            rate.to_string(),
            "Old Card does not run at 48,000 Hz: it runs at 44100 Hz, 8000 to 32000 Hz"
        );
        let form = DeviceError::Form {
            device: "Odd Card".to_owned(),
            form: "dsd8".to_owned(),
        };
        assert!(form.to_string().contains("Odd Card") && form.to_string().contains("dsd8"));
        assert!(
            DeviceError::Unknown("x:y".to_owned())
                .to_string()
                .contains("x:y")
        );
    }

    /// Needs a loudspeaker, and makes no sound: half a second of silence
    /// is played through the real device, to its end.
    #[test]
    #[ignore = "needs a sound device"]
    fn live_the_loudspeaker_plays_what_it_is_given_to_the_end() {
        let host = cpal::default_host();
        let device = host.default_output_device().expect("a loudspeaker");
        let offered = device
            .supported_output_configs()
            .expect("its ways of running");
        let (config, form) = choose(&device, offered, false).expect("a way of running it");
        println!("{}: {config:?} as {form}", name(&device));

        let (mut line, end) = line();
        let stream = with_form!(form, &device, play(&device, config, end.speaker, end.alarm))
            .expect("a stream");
        stream.play().expect("a stream that runs");

        assert!(line.play(1, core::iter::repeat_n(0, 24_000)));
        line.end(1);
        let all = Progress {
            run: 1,
            played: 24_000,
        };
        let began = Instant::now();
        while line.playing() != all {
            assert!(
                began.elapsed() < Duration::from_secs(5),
                "{:?}",
                line.playing()
            );
            assert!(!line.failed(), "{:?}", line.health().failure());
            std::thread::sleep(Duration::from_millis(5));
        }
        // At the pace of a clock, not as fast as the samples can be
        // taken: half a second of audio takes about half a second.
        let took = began.elapsed();
        println!(
            "played in {took:?}, with {} stumbles",
            line.health().stumbles()
        );
        assert!(took > Duration::from_millis(350), "{took:?}");
    }
}
