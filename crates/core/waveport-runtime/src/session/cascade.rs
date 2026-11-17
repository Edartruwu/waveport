//! The parts of a session in which speech is recognised, a language model
//! writes and a synthesiser speaks: what hears each participant, and the
//! session's voice.
//!
//! This is everything of such a session that a session of another shape
//! does not have. The language model is not here: every shape has one.

use core::fmt;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::Arc;

use waveport_domain::command::SenseCmd;
use waveport_domain::event::{Event, Key};
use waveport_domain::ids::{OpTag, ParticipantId, TurnId};
use waveport_domain::kernel::View;
use waveport_domain::time::{MediaDur, MediaTime, MonoTime};
use waveport_media::ingress::Packet;
use waveport_ports::plan::CascadePlan;
use waveport_ports::stt::{Stt, SttRx};
use waveport_ports::transport::Transport;
use waveport_ports::tts::Tts;

use super::hear::{Ear, HearStats};
use super::speak::{Milestone, Voice};
use super::{SessionStats, StartError};
use crate::compute::{Compute, Wants};
use crate::edges::EdgeConfig;

/// What a cascaded session hears with and speaks with.
pub(crate) struct Cascade {
    plan: CascadePlan,
    edges: EdgeConfig,
    language: Option<String>,
    wants: Wants,
    stt: Arc<dyn Stt>,
    compute: Arc<Compute>,
    ears: Vec<Ear>,
    voice: Voice,
    /// Recognisers of participants who are no longer listened to, told
    /// that no more audio follows and read until they end. What they
    /// still write down is for nobody.
    parting: Vec<Pin<Box<dyn SttRx>>>,
    /// Participants whose audio could not be set up, or for whom there
    /// was no room.
    unheard: u32,
    /// Packets from someone who had not joined.
    strays: u64,
    /// What went wrong while hearing, over all participants who have
    /// left.
    hearing: HearStats,
}

impl Cascade {
    /// The parts of a session that is run as `plan` says.
    ///
    /// # Errors
    ///
    /// The listening models take another rate than the session listens
    /// at, or the voice cannot be set up.
    pub(crate) fn new(
        plan: CascadePlan,
        stt: Arc<dyn Stt>,
        tts: Arc<dyn Tts>,
        compute: Arc<Compute>,
        edges: EdgeConfig,
        language: Option<String>,
    ) -> Result<Self, StartError> {
        let wants = compute.wants();
        if wants.rate != plan.listen.rate {
            return Err(StartError::ModelRate {
                models: wants.rate,
                session: plan.listen.rate,
            });
        }
        let voice = Voice::new(plan.speak, plan.playback, tts, language.clone())
            .map_err(StartError::Codec)?;
        Ok(Self {
            plan,
            edges,
            language,
            wants,
            stt,
            compute,
            ears: Vec::new(),
            voice,
            parting: Vec::new(),
            unheard: 0,
            strays: 0,
            hearing: HearStats::default(),
        })
    }

    /// Adds what went wrong so far to `stats`.
    pub(crate) fn count(&self, stats: &mut SessionStats) {
        stats.unheard = self.unheard;
        stats.strays = self.strays;
        stats.hearing = self.hearing;
        stats.speaking = self.voice.stats();
        for ear in &self.ears {
            let hearing = ear.stats();
            stats.hearing.recogniser_failures += hearing.recogniser_failures;
            stats.hearing.finalize_refused += hearing.finalize_refused;
        }
    }

    /// How far playback has got, if that has changed since the kernel was
    /// last told.
    pub(crate) fn position(&mut self, now: MonoTime) -> Option<Event> {
        self.voice.position(now)
    }

    /// A participant is there: from now on they are heard, if there is
    /// room for one more among `most`.
    pub(crate) fn joined(&mut self, who: ParticipantId, most: usize) {
        if self.ears.iter().any(|ear| ear.who == who) {
            return;
        }
        if self.ears.len() >= most {
            self.unheard += 1;
            return;
        }
        let ear = Ear::new(
            who,
            self.plan.listen,
            MonoTime::ZERO,
            self.compute.listen(),
            self.edges,
            Arc::clone(&self.stt),
            self.language.clone(),
        );
        match ear {
            Ok(ear) => self.ears.push(ear),
            Err(_) => self.unheard += 1,
        }
    }

    /// Takes a packet of a participant.
    pub(crate) fn packet(&mut self, now: MonoTime, who: ParticipantId, packet: Packet<'_>) {
        match self.ears.iter_mut().find(|ear| ear.who == who) {
            Some(ear) => ear.packet(now, packet),
            None => self.strays += 1,
        }
    }

    /// The far end says how much of a response it has played.
    pub(crate) fn acked(&mut self, tag: OpTag, played: MediaDur) {
        self.voice.acked(tag, played);
    }

    /// Moves on what hears the participants, and adds to `events` what
    /// the kernel is to be told. Returns whether anything changed that may
    /// have made more work.
    pub(crate) fn hear(
        &mut self,
        cx: &mut Context<'_>,
        now: MonoTime,
        events: &mut Vec<Event>,
    ) -> bool {
        let mut changed = false;
        for ear in &mut self.ears {
            changed |= ear.poll(cx, now, events);
        }
        self.part(cx);
        changed
    }

    /// Moves synthesis on. Returns whether anything changed that may have
    /// made more work.
    pub(crate) fn speak(
        &mut self,
        cx: &mut Context<'_>,
        now: MonoTime,
        events: &mut Vec<Event>,
        milestone: &mut impl FnMut(Milestone),
    ) -> bool {
        self.voice.poll(cx, now, events, milestone)
    }

    /// Hands the frames that are due to the transport. Returns whether
    /// there were any.
    pub(crate) fn send_frames(
        &mut self,
        now: MonoTime,
        transport: &mut dyn Transport,
        milestone: &mut impl FnMut(Milestone),
    ) -> bool {
        self.voice.send_frames(now, transport, milestone)
    }

    /// The next time something here is due that nothing will wake the
    /// session for.
    pub(crate) fn deadline(&self) -> Option<MonoTime> {
        self.ears
            .iter()
            .filter_map(Ear::deadline)
            .chain(self.voice.deadline())
            .min()
    }

    /// Stops hearing and speaking where they are, unasked: for a session
    /// that may say nothing more.
    pub(crate) fn halt(&mut self, fade_ms: u16, now: MonoTime, transport: &mut dyn Transport) {
        self.voice.halt(fade_ms, now, transport);
        self.ears.clear();
        self.parting.clear();
    }

    /// Stops listening to everyone who is still listened to.
    pub(crate) fn leave(&mut self) {
        for ear in core::mem::take(&mut self.ears) {
            self.let_go(ear);
        }
    }

    /// Reads the recognisers that were told that no more audio follows,
    /// and lets go of the ones that have ended. Returns whether none is
    /// left.
    pub(crate) fn parted(&mut self, cx: &mut Context<'_>) -> bool {
        self.part(cx);
        self.parting.is_empty()
    }

    /// The kernel releases a piece of the text of the response `tag`.
    /// Returns whether it is the first of that response.
    pub(crate) fn say(&mut self, tag: OpTag, piece: &str) -> bool {
        self.voice.speak(tag, piece)
    }

    /// The session is to press `keys`.
    pub(crate) fn press(&mut self, keys: &[Key], now: MonoTime) {
        self.voice.press(keys, now);
    }

    /// The keys that are still to be pressed are not.
    pub(crate) fn forget_keys(&mut self) {
        self.voice.forget_keys();
    }

    /// No more text of the response `tag` follows.
    pub(crate) fn finish(&mut self, tag: OpTag) {
        self.voice.finish(tag);
    }

    /// The synthesis of the response `tag` is not wanted any more.
    pub(crate) fn cancel(&mut self, tag: OpTag) {
        self.voice.cancel(tag);
    }

    /// Asks the recogniser of `who` for a final transcript.
    pub(crate) fn finalize(&mut self, who: ParticipantId) {
        if let Some(ear) = self.ears.iter_mut().find(|ear| ear.who == who) {
            ear.finalize();
        }
    }

    /// Stops listening to `who`.
    pub(crate) fn close(&mut self, who: ParticipantId) {
        if let Some(index) = self.ears.iter().position(|ear| ear.who == who) {
            let ear = self.ears.remove(index);
            self.let_go(ear);
        }
    }

    /// Asks the listening models what the kernel wants to know of a turn.
    pub(crate) fn sense(&mut self, view: View<'_>, command: SenseCmd) {
        match command {
            SenseCmd::EstimateEou { turn, who } => {
                self.judge(view, turn, who, self.wants.turn_window, false);
            }
            SenseCmd::ClassifyOverlap { turn, who } => {
                if let Some(window) = self.wants.overlap_window {
                    self.judge(view, turn, who, window, true);
                }
            }
        }
    }

    pub(crate) fn start(&mut self, tag: OpTag, now: MonoTime) {
        self.voice.start(tag, now);
    }

    pub(crate) fn duck(&mut self, tag: OpTag) {
        self.voice.duck(tag);
    }

    pub(crate) fn unduck(&mut self, tag: OpTag) {
        self.voice.unduck(tag);
    }

    /// Stops playing the response `tag`.
    pub(crate) fn stop(
        &mut self,
        tag: OpTag,
        fade_ms: u16,
        now: MonoTime,
        transport: &mut dyn Transport,
    ) {
        self.voice.stop(tag, fade_ms, now, transport);
    }

    /// Stops listening to a participant: what went wrong while they were
    /// heard is kept, and their recogniser is told that no more audio
    /// follows.
    fn let_go(&mut self, ear: Ear) {
        let (hearing, left) = ear.leave();
        self.hearing.recogniser_failures += hearing.recogniser_failures;
        self.hearing.finalize_refused += hearing.finalize_refused;
        self.parting.extend(left);
    }

    /// Reads the recognisers that were told that no more audio follows,
    /// and lets go of the ones that have ended.
    fn part(&mut self, cx: &mut Context<'_>) {
        self.parting.retain_mut(|left| {
            loop {
                match left.as_mut().poll_next(cx) {
                    Poll::Ready(Some(Ok(_))) => {}
                    Poll::Ready(Some(Err(_)) | None) => return false,
                    Poll::Pending => return true,
                }
            }
        });
    }

    /// Asks the listening models about a turn.
    fn judge(
        &mut self,
        view: View<'_>,
        turn: TurnId,
        who: ParticipantId,
        window: usize,
        overlap: bool,
    ) {
        let began = view
            .turns()
            .find(|open| open.turn == turn)
            .map_or(MediaTime::from_ticks(0), |open| open.span.start);
        if let Some(ear) = self.ears.iter_mut().find(|ear| ear.who == who) {
            ear.judge(turn, began, window, overlap);
        }
    }
}

impl fmt::Debug for Cascade {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cascade")
            .field("ears", &self.ears)
            .field("voice", &self.voice)
            .finish_non_exhaustive()
    }
}
