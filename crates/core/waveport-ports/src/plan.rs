//! The session planner: from what the parts can do to how the session is
//! run.
//!
//! Providers differ, and a session cannot find out halfway through a
//! conversation that its recogniser gives no times or that its transport
//! cannot take audio back. [`plan`] is called before a session starts,
//! with what each part says it can do. It decides everything that depends
//! on that, once, and it is a pure function, so every combination can be
//! tested without a provider.
//!
//! It fails loudly and never degrades in silence. What cannot work at all
//! is a [`PlanError`]. What works, but less well than it would with other
//! parts, is listed in the plan as a [`Shortfall`], for the application
//! to log, to show, or to refuse.

use core::fmt;

use crate::audio::{Codec, Rate};
use crate::llm::LlmCaps;
use crate::stt::{SttCaps, SttFinalize, SttTiming};
use crate::transport::TransportCaps;
use crate::tts::{Alignment, TtsCaps, TtsInput};

/// The rate of the audio the listening models take, and so the rate a
/// session's inbound audio is brought to.
pub const LISTEN_RATE: Rate = Rate::Hz16000;

/// The rate speech is synthesised at when nothing stands against it.
/// Speech has little above 12 kHz, so a higher rate would cost twice the
/// bytes and the work for nothing a listener hears.
pub const SPEECH_RATE: Rate = Rate::Hz24000;

/// Which parts fill the three roles of an agent: hearing, thinking and
/// speaking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrainProfile {
    /// A part for each role: a speech recogniser, a language model and a
    /// speech synthesiser.
    Cascaded {
        /// The speech recogniser.
        hear: SttCaps,
        /// The language model.
        think: LlmCaps,
        /// The speech synthesiser.
        speak: TtsCaps,
    },
    /// A language model alone: a session of text, in which nothing is
    /// heard and nothing is spoken.
    Text {
        /// The language model.
        think: LlmCaps,
    },
    /// One speech-to-speech model in all three roles.
    Realtime,
}

/// What an agent needs of its parts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct AgentNeeds {
    /// How many tools it has.
    pub tools: usize,
}

/// A role in an agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// Speech recognition.
    Hear,
    /// The language model.
    Think,
    /// Speech synthesis.
    Speak,
}

/// Why a session cannot be run with these parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlanError {
    /// Speech-to-speech models are not supported yet.
    Realtime,
    /// The part in this role works at none of the rates the media path
    /// does.
    NoRate(Role),
    /// The agent has tools and the language model cannot call any.
    NoTools,
    /// The parts hear and speak, and the session has no transport that
    /// carries audio: it has none at all, or one for text alone.
    NoAudio,
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Realtime => f.write_str("speech-to-speech models are not supported yet"),
            Self::NoRate(Role::Hear) => {
                f.write_str("the speech recogniser takes audio at none of 8, 16, 24 and 48 kHz")
            }
            Self::NoRate(_) => {
                f.write_str("the speech synthesiser gives audio at none of 8, 16, 24 and 48 kHz")
            }
            Self::NoTools => {
                f.write_str("the agent has tools and the language model cannot call any")
            }
            Self::NoAudio => f.write_str(
                "the parts hear and speak, and the session has no transport that carries audio",
            ),
        }
    }
}

impl core::error::Error for PlanError {}

/// How a session is run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionPlan {
    /// With a speech recogniser, a language model and a speech
    /// synthesiser.
    Cascaded(CascadePlan),
    /// With a language model alone: what is typed is answered in text.
    Text(TextPlan),
}

impl SessionPlan {
    /// What the session does less well than it would with other parts.
    #[must_use]
    pub fn shortfalls(&self) -> &[Shortfall] {
        match self {
            Self::Cascaded(plan) => &plan.shortfalls,
            Self::Text(plan) => &plan.shortfalls,
        }
    }
}

/// How a session of text alone is run. Nothing of it depends on what its
/// parts can do, beyond whether it can be run at all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextPlan {
    /// What the session does less well than it would with other parts.
    pub shortfalls: Vec<Shortfall>,
}

/// How a session with a part for each role is run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CascadePlan {
    /// How it listens.
    pub listen: ListenPlan,
    /// How it speaks.
    pub speak: SpeakPlan,
    /// How it knows what was heard, and how it stops.
    pub playback: PlaybackPlan,
    /// What it does less well than it would with other parts.
    pub shortfalls: Vec<Shortfall>,
}

/// How a session listens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ListenPlan {
    /// How the audio that arrives is written.
    pub wire: Codec,
    /// The rate the inbound path brings it to, for the listening models.
    pub rate: Rate,
    /// The rate the speech recogniser is sent it at. Where this is not
    /// [`rate`](Self::rate), the rate is changed on the way.
    pub recogniser_rate: Rate,
    /// Whether the recogniser is asked for a final transcript when a turn
    /// seems to have ended. When not, it decides for itself, and asking
    /// would do nothing.
    pub ask_to_finalize: bool,
}

/// How a session speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SpeakPlan {
    /// The rate speech is synthesised and played out at.
    pub rate: Rate,
    /// How the audio to send has to be written. Where its rate is not
    /// [`rate`](Self::rate), the rate is changed on the way.
    pub wire: Codec,
    /// How the marks of the synthesiser are read.
    pub marks: Marks,
    /// How the synthesiser wants its text.
    pub input: TtsInput,
}

/// How the marks of a speech synthesiser are read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Marks {
    /// Each mark is a word or a character with its time: the text up to
    /// it is heard when its audio has been played.
    Timed,
    /// Marks are far apart, at the end of a piece of text or of all of
    /// it. The text between two marks is taken to be spread evenly over
    /// the audio between them.
    Spread,
}

/// How a session knows what was heard, and how it stops speaking.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlaybackPlan {
    /// Whether what was played is what the far end says was played. When
    /// not, it is worked out from the pace at which audio was sent.
    pub far_end_reports: bool,
    /// Whether, at an interruption, the far end is told to discard the
    /// audio it holds.
    pub clear_on_stop: bool,
}

/// Something a session does less well with these parts than it would
/// with others.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Shortfall {
    /// The recogniser gives no times for its words. A transcript is
    /// joined to a turn by the audio that was sent, which is wider than
    /// the words.
    WideTranscriptSpans,
    /// The recogniser decides for itself when a transcript is final, and
    /// cannot be asked. A turn that has ended waits for its words for as
    /// long as the recogniser takes to notice.
    TranscriptsNotOnRequest,
    /// The synthesiser does not say where the words are in its audio. An
    /// interrupted response is cut by share of characters, and the cut
    /// can be a few words away from what was heard. That is so inside a
    /// piece of text whose audio has all arrived. Inside one whose audio
    /// is still arriving nothing is known yet, and the cut is where the
    /// piece begins. For a synthesiser that is given the text of a
    /// response bit by bit in one stream, the whole response is such a
    /// piece until the synthesiser has finished: a response that is
    /// interrupted before that is recorded as not heard at all.
    CoarseSpokenText,
    /// The far end does not say what it has played. What was heard is
    /// worked out from the pace, and is ahead of the truth by whatever
    /// the far end holds.
    PlaybackEstimated,
    /// Audio that was sent cannot be taken back. After an interruption
    /// the listener still hears what the far end holds.
    CannotClear,
    /// The session is of text alone and its transport carries audio.
    /// Nothing that is said aloud is heard.
    AudioUnheard,
}

impl fmt::Display for Shortfall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::WideTranscriptSpans => {
                "the speech recogniser gives no times: transcripts are joined to turns by the \
                 audio sent, not by the words"
            }
            Self::TranscriptsNotOnRequest => {
                "the speech recogniser cannot be asked for a final transcript: a turn that \
                 has ended waits until the recogniser notices"
            }
            Self::CoarseSpokenText => {
                "the speech synthesiser gives no word times: an interrupted response is cut \
                 by share of characters"
            }
            Self::PlaybackEstimated => {
                "the far end does not say what it has played: what was heard is worked out \
                 from the pace"
            }
            Self::CannotClear => {
                "audio that was sent cannot be taken back: after an interruption the \
                 listener still hears what the far end holds"
            }
            Self::AudioUnheard => {
                "the session is of text alone and its transport carries audio: nothing \
                 that is said aloud is heard"
            }
        })
    }
}

/// Decides how a session is run with the parts it has. `transport` is
/// what its transport can do, or `None` for a session that has none: one
/// that is reached through the application alone.
///
/// # Errors
///
/// The session cannot be run with these parts at all: see [`PlanError`].
pub fn plan(
    agent: &AgentNeeds,
    brain: &BrainProfile,
    transport: Option<&TransportCaps>,
) -> Result<SessionPlan, PlanError> {
    let (hear, think, speak) = match brain {
        BrainProfile::Cascaded { hear, think, speak } => (hear, think, speak),
        BrainProfile::Text { think } => return written(*agent, *think, transport),
        BrainProfile::Realtime => return Err(PlanError::Realtime),
    };
    if agent.tools > 0 && !think.tools {
        return Err(PlanError::NoTools);
    }
    let Some(transport) = transport.and_then(|transport| transport.audio) else {
        return Err(PlanError::NoAudio);
    };
    let mut shortfalls = Vec::new();

    // Listening. The models decide the rate of the inbound path; the
    // recogniser is given the same audio, at the nearest rate it takes.
    let recogniser_rate = hear
        .rates
        .nearest(LISTEN_RATE)
        .ok_or(PlanError::NoRate(Role::Hear))?;
    if hear.timing == SttTiming::Window {
        shortfalls.push(Shortfall::WideTranscriptSpans);
    }
    if hear.finalize == SttFinalize::ByProvider {
        shortfalls.push(Shortfall::TranscriptsNotOnRequest);
    }
    let listen = ListenPlan {
        wire: transport.inbound,
        rate: LISTEN_RATE,
        recogniser_rate,
        ask_to_finalize: hear.finalize.on_request(),
    };

    // Speaking. There is nothing to gain from synthesising more than the
    // wire can carry.
    let wanted = SPEECH_RATE.min(transport.outbound.widest());
    let rate = speak
        .rates
        .nearest(wanted)
        .ok_or(PlanError::NoRate(Role::Speak))?;
    let marks = if speak.alignment == Alignment::None {
        shortfalls.push(Shortfall::CoarseSpokenText);
        Marks::Spread
    } else {
        Marks::Timed
    };
    let speak = SpeakPlan {
        rate,
        wire: transport.outbound,
        marks,
        input: speak.input,
    };

    // Playback.
    let buffer = transport.remote_buffer;
    if !buffer.reports_played {
        shortfalls.push(Shortfall::PlaybackEstimated);
    }
    if !buffer.clearable {
        shortfalls.push(Shortfall::CannotClear);
    }
    let playback = PlaybackPlan {
        far_end_reports: buffer.reports_played,
        clear_on_stop: buffer.clearable,
    };

    Ok(SessionPlan::Cascaded(CascadePlan {
        listen,
        speak,
        playback,
        shortfalls,
    }))
}

/// How a session of text alone is run.
fn written(
    agent: AgentNeeds,
    think: LlmCaps,
    transport: Option<&TransportCaps>,
) -> Result<SessionPlan, PlanError> {
    if agent.tools > 0 && !think.tools {
        return Err(PlanError::NoTools);
    }
    let mut shortfalls = Vec::new();
    if transport.is_some_and(|transport| transport.audio.is_some()) {
        shortfalls.push(Shortfall::AudioUnheard);
    }
    Ok(SessionPlan::Text(TextPlan { shortfalls }))
}

#[cfg(test)]
mod tests {
    use super::{
        AgentNeeds, BrainProfile, CascadePlan, LISTEN_RATE, Marks, PlanError, Role, SessionPlan,
        Shortfall, TextPlan, plan,
    };
    use crate::audio::{Codec, Law, Rate, RateSet};
    use crate::llm::LlmCaps;
    use crate::stt::{SttCaps, SttFinalize, SttTiming};
    use crate::transport::{AudioCaps, RemoteBuffer, TransportCaps};
    use crate::tts::{Alignment, TtsCaps, TtsInput};

    /// A recogniser with nothing to wish for.
    const HEAR: SttCaps = SttCaps {
        rates: RateSet::of(&[Rate::Hz8000, Rate::Hz16000]),
        interim: true,
        timing: SttTiming::Words,
        finalize: SttFinalize::Both,
    };
    const THINK: LlmCaps = LlmCaps {
        tools: true,
        parallel_tools: true,
    };
    /// A synthesiser with nothing to wish for.
    const SPEAK: TtsCaps = TtsCaps {
        rates: RateSet::of(&[Rate::Hz8000, Rate::Hz16000, Rate::Hz24000]),
        input: TtsInput::Incremental,
        alignment: Alignment::Word,
    };
    /// A sound card: plain samples, and the far end is this process.
    const CARD: TransportCaps = TransportCaps {
        audio: Some(CARD_AUDIO),
        text: false,
    };
    const CARD_AUDIO: AudioCaps = AudioCaps {
        inbound: Codec::Pcm(Rate::Hz48000),
        outbound: Codec::Pcm(Rate::Hz48000),
        remote_buffer: RemoteBuffer {
            clearable: true,
            reports_played: true,
        },
    };
    /// A telephone line that can neither take audio back nor say what it
    /// played.
    const LINE: TransportCaps = TransportCaps {
        audio: Some(AudioCaps {
            inbound: Codec::G711(Law::Mu),
            outbound: Codec::G711(Law::Mu),
            remote_buffer: RemoteBuffer {
                clearable: false,
                reports_played: false,
            },
        }),
        text: false,
    };

    fn brain(hear: SttCaps, think: LlmCaps, speak: TtsCaps) -> BrainProfile {
        BrainProfile::Cascaded { hear, think, speak }
    }

    fn cascade(agent: AgentNeeds, brain: &BrainProfile, transport: TransportCaps) -> CascadePlan {
        let SessionPlan::Cascaded(plan) = plan(&agent, brain, Some(&transport)).unwrap() else {
            panic!("parts that hear and speak are run as a cascade");
        };
        plan
    }

    #[test]
    fn parts_with_nothing_to_wish_for_leave_no_shortfall() {
        let agent = AgentNeeds { tools: 2 };
        let plan = cascade(agent, &brain(HEAR, THINK, SPEAK), CARD);
        assert_eq!(plan.shortfalls, []);
        assert_eq!(plan.listen.wire, Codec::Pcm(Rate::Hz48000));
        assert_eq!(plan.listen.rate, LISTEN_RATE);
        assert_eq!(plan.listen.recogniser_rate, Rate::Hz16000);
        assert!(plan.listen.ask_to_finalize);
        assert_eq!(plan.speak.rate, Rate::Hz24000);
        assert_eq!(plan.speak.marks, Marks::Timed);
        assert_eq!(plan.speak.input, TtsInput::Incremental);
        assert!(plan.playback.far_end_reports && plan.playback.clear_on_stop);
    }

    #[test]
    fn the_recogniser_is_sent_the_nearest_rate_it_takes() {
        let cases = [
            (&[Rate::Hz16000][..], Rate::Hz16000),
            // Nothing is lost by going up.
            (&[Rate::Hz8000, Rate::Hz24000], Rate::Hz24000),
            (&[Rate::Hz24000, Rate::Hz48000], Rate::Hz24000),
            // With nothing at or above, the highest below.
            (&[Rate::Hz8000], Rate::Hz8000),
        ];
        for (rates, expected) in cases {
            let hear = SttCaps {
                rates: RateSet::of(rates),
                ..HEAR
            };
            let plan = cascade(AgentNeeds::default(), &brain(hear, THINK, SPEAK), CARD);
            assert_eq!(plan.listen.recogniser_rate, expected, "{rates:?}");
            assert_eq!(plan.listen.rate, LISTEN_RATE, "the models' rate is fixed");
        }
    }

    #[test]
    fn speech_is_synthesised_at_no_more_than_the_wire_carries() {
        let all = RateSet::ALL;
        let only = |rate| RateSet::of(&[rate]);
        let cases = [
            // A telephone line carries 8 kHz.
            (all, LINE, Rate::Hz8000),
            (only(Rate::Hz24000), LINE, Rate::Hz24000),
            // Anything wider gets 24 kHz, where the synthesiser has it.
            (all, CARD, Rate::Hz24000),
            (only(Rate::Hz48000), CARD, Rate::Hz48000),
            (only(Rate::Hz16000), CARD, Rate::Hz16000),
        ];
        for (rates, transport, expected) in cases {
            let speak = TtsCaps { rates, ..SPEAK };
            let plan = cascade(AgentNeeds::default(), &brain(HEAR, THINK, speak), transport);
            assert_eq!(plan.speak.rate, expected, "{rates:?} on {transport:?}");
            assert_eq!(
                Some(plan.speak.wire),
                transport.audio.map(|audio| audio.outbound)
            );
        }
        let opus = TransportCaps {
            audio: Some(AudioCaps {
                outbound: Codec::Opus,
                ..CARD_AUDIO
            }),
            ..CARD
        };
        let plan = cascade(AgentNeeds::default(), &brain(HEAR, THINK, SPEAK), opus);
        assert_eq!(plan.speak.rate, Rate::Hz24000);
    }

    #[test]
    fn every_weakness_of_a_part_is_named() {
        let hear = SttCaps {
            timing: SttTiming::Window,
            finalize: SttFinalize::ByProvider,
            ..HEAR
        };
        let speak = TtsCaps {
            alignment: Alignment::None,
            input: TtsInput::Whole,
            ..SPEAK
        };
        let plan = cascade(AgentNeeds::default(), &brain(hear, THINK, speak), LINE);
        assert_eq!(
            plan.shortfalls,
            [
                Shortfall::WideTranscriptSpans,
                Shortfall::TranscriptsNotOnRequest,
                Shortfall::CoarseSpokenText,
                Shortfall::PlaybackEstimated,
                Shortfall::CannotClear,
            ]
        );
        assert!(!plan.listen.ask_to_finalize);
        assert_eq!(plan.speak.marks, Marks::Spread);
        assert_eq!(plan.speak.input, TtsInput::Whole);
        assert!(!plan.playback.far_end_reports && !plan.playback.clear_on_stop);
        // Each one says what it means for the session.
        for shortfall in &plan.shortfalls {
            assert!(shortfall.to_string().len() > 40, "{shortfall:?}");
        }
        let whole = SessionPlan::Cascaded(plan.clone());
        assert_eq!(whole.shortfalls(), plan.shortfalls);
    }

    #[test]
    fn one_weakness_is_one_shortfall() {
        let on_request = SttCaps {
            finalize: SttFinalize::OnRequest,
            ..HEAR
        };
        let plan = cascade(
            AgentNeeds::default(),
            &brain(on_request, THINK, SPEAK),
            CARD,
        );
        assert_eq!(plan.shortfalls, [], "asking is enough");
        assert!(plan.listen.ask_to_finalize);

        let chars = TtsCaps {
            alignment: Alignment::Char,
            ..SPEAK
        };
        let plan = cascade(AgentNeeds::default(), &brain(HEAR, THINK, chars), CARD);
        assert_eq!((plan.shortfalls.len(), plan.speak.marks), (0, Marks::Timed));

        let no_reports = TransportCaps {
            audio: Some(AudioCaps {
                remote_buffer: RemoteBuffer {
                    clearable: true,
                    reports_played: false,
                },
                ..CARD_AUDIO
            }),
            ..CARD
        };
        let plan = cascade(
            AgentNeeds::default(),
            &brain(HEAR, THINK, SPEAK),
            no_reports,
        );
        assert_eq!(plan.shortfalls, [Shortfall::PlaybackEstimated]);
        assert!(plan.playback.clear_on_stop);
    }

    #[test]
    fn a_session_of_text_needs_a_language_model_and_nothing_else() {
        let agent = AgentNeeds::default();
        let text = BrainProfile::Text { think: THINK };
        // No transport, or one for text alone, leaves nothing to wish for.
        let nothing_amiss = Ok(SessionPlan::Text(TextPlan::default()));
        assert_eq!(plan(&agent, &text, None), nothing_amiss);
        let written = TransportCaps {
            audio: None,
            text: true,
        };
        assert_eq!(plan(&agent, &text, Some(&written)), nothing_amiss);

        // On a transport that carries audio nobody is heard, and the plan
        // says so.
        let on_a_line = plan(&agent, &text, Some(&LINE)).unwrap();
        assert_eq!(on_a_line.shortfalls(), [Shortfall::AudioUnheard]);
        assert!(Shortfall::AudioUnheard.to_string().len() > 40);

        // Tools need a model that can call them, as in every shape.
        let no_tools = BrainProfile::Text {
            think: LlmCaps {
                tools: false,
                parallel_tools: false,
            },
        };
        assert_eq!(
            plan(&AgentNeeds { tools: 1 }, &no_tools, None),
            Err(PlanError::NoTools)
        );
        assert!(plan(&agent, &no_tools, None).is_ok());
    }

    #[test]
    fn what_cannot_work_is_refused() {
        let agent = AgentNeeds::default();
        assert_eq!(
            plan(&agent, &BrainProfile::Realtime, Some(&CARD)),
            Err(PlanError::Realtime)
        );

        let deaf = SttCaps {
            rates: RateSet::NONE,
            ..HEAR
        };
        assert_eq!(
            plan(&agent, &brain(deaf, THINK, SPEAK), Some(&CARD)),
            Err(PlanError::NoRate(Role::Hear))
        );
        let mute = TtsCaps {
            rates: RateSet::NONE,
            ..SPEAK
        };
        assert_eq!(
            plan(&agent, &brain(HEAR, THINK, mute), Some(&CARD)),
            Err(PlanError::NoRate(Role::Speak))
        );

        let no_tools = LlmCaps {
            tools: false,
            parallel_tools: false,
        };
        let with_tools = AgentNeeds { tools: 1 };
        assert_eq!(
            plan(&with_tools, &brain(HEAR, no_tools, SPEAK), Some(&CARD)),
            Err(PlanError::NoTools)
        );
        // An agent without tools does not need a model that can call them.
        assert!(plan(&agent, &brain(HEAR, no_tools, SPEAK), Some(&CARD)).is_ok());
        // One call at a time is enough.
        let one_at_a_time = LlmCaps {
            tools: true,
            parallel_tools: false,
        };
        assert!(plan(&with_tools, &brain(HEAR, one_at_a_time, SPEAK), Some(&CARD)).is_ok());

        // Parts that hear and speak need a transport, and one that
        // carries audio.
        let written = TransportCaps {
            audio: None,
            text: true,
        };
        assert_eq!(
            plan(&agent, &brain(HEAR, THINK, SPEAK), None),
            Err(PlanError::NoAudio)
        );
        assert_eq!(
            plan(&agent, &brain(HEAR, THINK, SPEAK), Some(&written)),
            Err(PlanError::NoAudio)
        );

        for error in [
            PlanError::Realtime,
            PlanError::NoRate(Role::Hear),
            PlanError::NoRate(Role::Speak),
            PlanError::NoTools,
            PlanError::NoAudio,
        ] {
            assert!(error.to_string().len() > 30, "{error:?}");
        }
    }
}
