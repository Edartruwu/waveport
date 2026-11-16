//! What a session does differently for the shape it has.
//!
//! Every session has a kernel, timers, tools, a journal, followers and a
//! language model. What it hears with and what it speaks with depend on
//! its shape: a session in which speech is recognised and synthesised has
//! ears and a voice, and one of text alone has neither. A [`Driver`] is
//! those parts, behind the calls the session makes of them. It is an enum
//! and not a trait object: the shapes are few, they are all known here,
//! and every step of every session goes through it.

use core::task::Context;

use waveport_domain::command::SenseCmd;
use waveport_domain::event::Event;
use waveport_domain::ids::{OpTag, ParticipantId};
use waveport_domain::kernel::View;
use waveport_domain::time::{MediaDur, MonoTime};
use waveport_media::ingress::Packet;
use waveport_ports::transport::Transport;

use super::SessionStats;
use super::cascade::Cascade;
use super::speak::Milestone;

/// The parts of a session that depend on its shape.
#[derive(Debug)]
pub(crate) enum Driver {
    /// Speech is recognised, a language model writes, a synthesiser
    /// speaks.
    Cascade(Cascade),
}

impl Driver {
    /// Adds what went wrong so far to `stats`.
    pub(crate) fn count(&self, stats: &mut SessionStats) {
        match self {
            Self::Cascade(parts) => parts.count(stats),
        }
    }

    /// How far what the agent says has got with whoever it is for, if
    /// that has changed since the kernel was last told.
    pub(crate) fn position(&mut self, now: MonoTime) -> Option<Event> {
        match self {
            Self::Cascade(parts) => parts.position(now),
        }
    }

    /// A participant is there. No more than `most` are heard at a time.
    pub(crate) fn joined(&mut self, who: ParticipantId, most: usize) {
        match self {
            Self::Cascade(parts) => parts.joined(who, most),
        }
    }

    /// Takes a packet of a participant's audio.
    pub(crate) fn packet(&mut self, now: MonoTime, who: ParticipantId, packet: Packet<'_>) {
        match self {
            Self::Cascade(parts) => parts.packet(now, who, packet),
        }
    }

    /// The far end says how much of a response it has played.
    pub(crate) fn acked(&mut self, tag: OpTag, played: MediaDur) {
        match self {
            Self::Cascade(parts) => parts.acked(tag, played),
        }
    }

    /// Moves on what takes in what the participants say, and adds to
    /// `events` what the kernel is to be told. Returns whether anything
    /// changed that may have made more work.
    pub(crate) fn hear(
        &mut self,
        cx: &mut Context<'_>,
        now: MonoTime,
        events: &mut Vec<Event>,
    ) -> bool {
        match self {
            Self::Cascade(parts) => parts.hear(cx, now, events),
        }
    }

    /// Moves on what brings the agent's words to the participants.
    /// Returns whether anything changed that may have made more work.
    pub(crate) fn speak(
        &mut self,
        cx: &mut Context<'_>,
        now: MonoTime,
        events: &mut Vec<Event>,
        milestone: &mut impl FnMut(Milestone),
    ) -> bool {
        match self {
            Self::Cascade(parts) => parts.speak(cx, now, events, milestone),
        }
    }

    /// Hands the transport what is due to go out. Returns whether there
    /// was anything.
    pub(crate) fn send_frames(
        &mut self,
        now: MonoTime,
        transport: &mut dyn Transport,
        milestone: &mut impl FnMut(Milestone),
    ) -> bool {
        match self {
            Self::Cascade(parts) => parts.send_frames(now, transport, milestone),
        }
    }

    /// The next time something here is due that nothing will wake the
    /// session for.
    pub(crate) fn deadline(&self) -> Option<MonoTime> {
        match self {
            Self::Cascade(parts) => parts.deadline(),
        }
    }

    /// Stops hearing and speaking where they are, unasked: for a session
    /// that may say nothing more.
    pub(crate) fn halt(&mut self, fade_ms: u16, now: MonoTime, transport: &mut dyn Transport) {
        match self {
            Self::Cascade(parts) => parts.halt(fade_ms, now, transport),
        }
    }

    /// Stops listening to everyone who is still listened to: the session
    /// has closed.
    pub(crate) fn leave(&mut self) {
        match self {
            Self::Cascade(parts) => parts.leave(),
        }
    }

    /// Whether everything that was let go of has ended.
    pub(crate) fn parted(&mut self, cx: &mut Context<'_>) -> bool {
        match self {
            Self::Cascade(parts) => parts.parted(cx),
        }
    }

    /// The kernel releases a piece of the text of the response `tag`.
    /// Returns whether it is the first of that response.
    pub(crate) fn say(&mut self, tag: OpTag, piece: &str) -> bool {
        match self {
            Self::Cascade(parts) => parts.say(tag, piece),
        }
    }

    /// No more text of the response `tag` follows.
    pub(crate) fn finish(&mut self, tag: OpTag) {
        match self {
            Self::Cascade(parts) => parts.finish(tag),
        }
    }

    /// What was made of the text of the response `tag` is not wanted any
    /// more.
    pub(crate) fn cancel(&mut self, tag: OpTag) {
        match self {
            Self::Cascade(parts) => parts.cancel(tag),
        }
    }

    /// The words of `who` so far are asked for as final.
    pub(crate) fn finalize(&mut self, who: ParticipantId) {
        match self {
            Self::Cascade(parts) => parts.finalize(who),
        }
    }

    /// `who` is not listened to any more.
    pub(crate) fn close(&mut self, who: ParticipantId) {
        match self {
            Self::Cascade(parts) => parts.close(who),
        }
    }

    /// The kernel wants a judgement on a turn.
    pub(crate) fn sense(&mut self, view: View<'_>, command: SenseCmd) {
        match self {
            Self::Cascade(parts) => parts.sense(view, command),
        }
    }

    /// The response `tag` may be heard from now on.
    pub(crate) fn start(&mut self, tag: OpTag, now: MonoTime) {
        match self {
            Self::Cascade(parts) => parts.start(tag, now),
        }
    }

    /// The response `tag` is to be quieter.
    pub(crate) fn duck(&mut self, tag: OpTag) {
        match self {
            Self::Cascade(parts) => parts.duck(tag),
        }
    }

    /// The response `tag` is to be as loud as it was.
    pub(crate) fn unduck(&mut self, tag: OpTag) {
        match self {
            Self::Cascade(parts) => parts.unduck(tag),
        }
    }

    /// The response `tag` is to be heard no further.
    pub(crate) fn stop(
        &mut self,
        tag: OpTag,
        fade_ms: u16,
        now: MonoTime,
        transport: &mut dyn Transport,
    ) {
        match self {
            Self::Cascade(parts) => parts.stop(tag, fade_ms, now, transport),
        }
    }
}
