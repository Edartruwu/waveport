//! What a session does differently for the shape it has.
//!
//! Every session has a kernel, timers, tools, a journal, followers and a
//! language model. What it hears with and what it speaks with depend on
//! its shape: a session in which speech is recognised and synthesised has
//! ears and a voice, and one of text alone has neither. A [`Driver`] is
//! those parts, behind the calls the session makes of them. It is an enum
//! and not a trait object: the shapes are few, they are all known here,
//! and every step of every session goes through it.
//!
//! The kernel asks the same of every shape. What a shape has no part for
//! it does nothing about: a session of text is asked to make a response
//! quieter and has nothing to turn down.

use core::task::Context;

use waveport_domain::command::SenseCmd;
use waveport_domain::event::{Event, Key};
use waveport_domain::ids::{OpTag, ParticipantId};
use waveport_domain::kernel::View;
use waveport_domain::time::{MediaDur, MonoTime};
use waveport_media::ingress::Packet;
use waveport_ports::transport::Transport;

use super::SessionStats;
use super::cascade::Cascade;
use super::speak::Milestone;
use super::write::Writer;

/// The parts of a session that depend on its shape.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "a session has one driver for as long as it lives and is never moved, and a box \
              would put a pointer between a call and its audio path. A session of text pays \
              for it with 900 bytes it does not use"
)]
pub(crate) enum Driver {
    /// Speech is recognised, a language model writes, a synthesiser
    /// speaks.
    Cascade(Cascade),
    /// A language model writes, and what it writes is read.
    Text(Writer),
}

/// What became of a piece of text the kernel released.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Said {
    /// Whether it is the first of its response.
    pub first: bool,
    /// Whether it has gone out: to speech synthesis, or to the reader.
    /// Text that has not waits until its response may be read.
    pub out: bool,
}

impl Driver {
    /// Adds what went wrong so far to `stats`.
    pub(crate) fn count(&self, stats: &mut SessionStats) {
        match self {
            Self::Cascade(parts) => parts.count(stats),
            Self::Text(writer) => {
                stats.strays = writer.unheard();
                stats.speaking.keys_unpressed = writer.unpressed();
            }
        }
    }

    /// How far what the agent says has got with whoever it is for, if
    /// that has changed since the kernel was last told.
    pub(crate) fn position(&mut self, now: MonoTime) -> Option<Event> {
        match self {
            Self::Cascade(parts) => parts.position(now),
            // What was read is told at once: see `settle`.
            Self::Text(_) => None,
        }
    }

    /// A participant is there. No more than `most` are heard at a time.
    pub(crate) fn joined(&mut self, who: ParticipantId, most: usize) {
        match self {
            Self::Cascade(parts) => parts.joined(who, most),
            Self::Text(_) => {}
        }
    }

    /// Takes a packet of a participant's audio.
    pub(crate) fn packet(&mut self, now: MonoTime, who: ParticipantId, packet: Packet<'_>) {
        match self {
            Self::Cascade(parts) => parts.packet(now, who, packet),
            Self::Text(writer) => writer.packet(),
        }
    }

    /// The far end says how much of a response it has played.
    pub(crate) fn acked(&mut self, tag: OpTag, played: MediaDur) {
        match self {
            Self::Cascade(parts) => parts.acked(tag, played),
            Self::Text(_) => {}
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
            Self::Text(_) => false,
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
            Self::Text(_) => false,
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
            Self::Text(_) => false,
        }
    }

    /// The next time something here is due that nothing will wake the
    /// session for.
    pub(crate) fn deadline(&self) -> Option<MonoTime> {
        match self {
            Self::Cascade(parts) => parts.deadline(),
            Self::Text(_) => None,
        }
    }

    /// What a step set in motion that takes no time, for the kernel to
    /// be told before anything else that has happened is looked at.
    /// Returns whether there was anything.
    pub(crate) fn settle(&mut self, events: &mut Vec<Event>) -> bool {
        match self {
            // Recognising, synthesising and playing take time, and say
            // what came of them when the session next looks.
            Self::Cascade(_) => false,
            Self::Text(writer) => writer.settle(events),
        }
    }

    /// Stops hearing and speaking where they are, unasked: for a session
    /// that may say nothing more.
    pub(crate) fn halt(&mut self, fade_ms: u16, now: MonoTime, transport: &mut dyn Transport) {
        match self {
            Self::Cascade(parts) => parts.halt(fade_ms, now, transport),
            Self::Text(writer) => writer.halt(),
        }
    }

    /// Stops listening to everyone who is still listened to: the session
    /// has closed.
    pub(crate) fn leave(&mut self) {
        match self {
            Self::Cascade(parts) => parts.leave(),
            Self::Text(_) => {}
        }
    }

    /// Whether everything that was let go of has ended.
    pub(crate) fn parted(&mut self, cx: &mut Context<'_>) -> bool {
        match self {
            Self::Cascade(parts) => parts.parted(cx),
            Self::Text(_) => true,
        }
    }

    /// The session is to press `keys` of its telephone.
    pub(crate) fn press(&mut self, keys: &[Key], now: MonoTime) {
        match self {
            Self::Cascade(parts) => parts.press(keys, now),
            // A session of text has no telephone.
            Self::Text(writer) => writer.cannot_press(keys.len()),
        }
    }

    /// The line is gone: the keys that are still to be pressed are not.
    pub(crate) fn forget_keys(&mut self) {
        match self {
            Self::Cascade(parts) => parts.forget_keys(),
            Self::Text(_) => {}
        }
    }

    /// The kernel releases a piece of the text of the response `tag`.
    pub(crate) fn say(&mut self, tag: OpTag, piece: &str) -> Said {
        match self {
            // What a session says aloud is told as it goes to synthesis.
            Self::Cascade(parts) => Said {
                first: parts.say(tag, piece),
                out: true,
            },
            Self::Text(writer) => writer.say(tag, piece),
        }
    }

    /// No more text of the response `tag` follows.
    pub(crate) fn finish(&mut self, tag: OpTag) {
        match self {
            Self::Cascade(parts) => parts.finish(tag),
            Self::Text(writer) => writer.finish(tag),
        }
    }

    /// What was made of the text of the response `tag` is not wanted any
    /// more.
    pub(crate) fn cancel(&mut self, tag: OpTag) {
        match self {
            Self::Cascade(parts) => parts.cancel(tag),
            Self::Text(writer) => writer.stop(tag),
        }
    }

    /// The words of `who` so far are asked for as final.
    pub(crate) fn finalize(&mut self, who: ParticipantId) {
        match self {
            Self::Cascade(parts) => parts.finalize(who),
            Self::Text(_) => {}
        }
    }

    /// `who` is not listened to any more.
    pub(crate) fn close(&mut self, who: ParticipantId) {
        match self {
            Self::Cascade(parts) => parts.close(who),
            Self::Text(_) => {}
        }
    }

    /// The kernel wants a judgement on a turn.
    pub(crate) fn sense(&mut self, view: View<'_>, command: SenseCmd) {
        match self {
            Self::Cascade(parts) => parts.sense(view, command),
            Self::Text(_) => {}
        }
    }

    /// The response `tag` may be heard from now on. Returns the text of
    /// it that was released before and has gone out only now.
    pub(crate) fn start(&mut self, tag: OpTag, now: MonoTime) -> Option<String> {
        match self {
            Self::Cascade(parts) => {
                parts.start(tag, now);
                None
            }
            Self::Text(writer) => writer.start(tag),
        }
    }

    /// The response `tag` is to be quieter.
    pub(crate) fn duck(&mut self, tag: OpTag) {
        match self {
            Self::Cascade(parts) => parts.duck(tag),
            Self::Text(_) => {}
        }
    }

    /// The response `tag` is to be as loud as it was.
    pub(crate) fn unduck(&mut self, tag: OpTag) {
        match self {
            Self::Cascade(parts) => parts.unduck(tag),
            Self::Text(_) => {}
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
            Self::Text(writer) => writer.stop(tag),
        }
    }
}
