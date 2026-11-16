//! How a session of text alone brings what its agent writes to whoever
//! reads it.
//!
//! Nothing is synthesised and nothing is played. To the kernel a response
//! is the same thing in every shape: text it releases a piece at a time,
//! which is said to be ready, which may be heard from some moment on, and
//! of which the kernel is told how far the listener has got. Here text is
//! ready as it is released, and the reader has it as soon as the response
//! may be read. "Played" means delivered, and a response that is cut off
//! is on record as far as it was delivered.
//!
//! The kernel is told at once, in the step after the one that released
//! the text, before anything else that has happened is looked at. A
//! reader has the text then, and whatever comes next, a message typed
//! over the answer or a tool that hands the floor to another agent, is
//! decided on that.
//!
//! Two things are kept in order for the kernel. Text has no audio, so
//! none is reported, and the limit on speech that waits to be played
//! never holds text back. And the report that all of a response has been
//! delivered follows the one that its last text is ready: told the other
//! way round, the kernel takes the report for one that came too early and
//! waits for another, which never comes.

use waveport_domain::event::{Event, OpEvent};
use waveport_domain::ids::OpTag;
use waveport_domain::time::MediaDur;

use super::drive::Said;

/// One response, as far as it has been written and read.
#[derive(Debug)]
struct Writing {
    tag: OpTag,
    /// Bytes of text the kernel has released.
    text: u32,
    /// Whether the kernel has said that no more text follows.
    finished: bool,
    /// Bytes of text the kernel has been told are ready, and whether it
    /// has been told that they are the last.
    counted: u32,
    ended: bool,
    /// Whether the response may be read yet.
    started: bool,
    /// Text that was released before it might be read.
    held: String,
    /// Bytes of text the reader has, and what the kernel was last told of
    /// that: how many, and whether they were all there will be.
    delivered: u32,
    reported: Option<(u32, bool)>,
}

/// The written side of a session of text.
#[derive(Debug, Default)]
pub(crate) struct Writer {
    writing: Option<Writing>,
    /// Packets of audio that came on a transport that carries some.
    /// Nobody hears them.
    unheard: u64,
}

impl Writer {
    /// The kernel releases a piece of the text of the response `tag`.
    pub(crate) fn say(&mut self, tag: OpTag, piece: &str) -> Said {
        let first = self
            .writing
            .as_ref()
            .is_none_or(|writing| writing.tag != tag);
        if first {
            self.writing = Some(Writing {
                tag,
                text: 0,
                finished: false,
                counted: 0,
                ended: false,
                started: false,
                held: String::new(),
                delivered: 0,
                reported: None,
            });
        }
        let Some(writing) = &mut self.writing else {
            return Said { first, out: false };
        };
        writing.text = writing
            .text
            .saturating_add(u32::try_from(piece.len()).unwrap_or(u32::MAX));
        if writing.started {
            writing.delivered = writing.text;
        } else {
            writing.held.push_str(piece);
        }
        Said {
            first,
            out: writing.started,
        }
    }

    /// A packet of audio came. A session of text hears none.
    pub(crate) const fn packet(&mut self) {
        self.unheard += 1;
    }

    /// How many packets of audio came and were not heard.
    pub(crate) const fn unheard(&self) -> u64 {
        self.unheard
    }

    /// No more text of the response `tag` follows.
    pub(crate) fn finish(&mut self, tag: OpTag) {
        if let Some(writing) = self.of(tag) {
            writing.finished = true;
        }
    }

    /// The response `tag` may be read from now on. Returns the text of it
    /// that was released before, which the reader has from now.
    pub(crate) fn start(&mut self, tag: OpTag) -> Option<String> {
        let writing = self.of(tag)?;
        if writing.started {
            return None;
        }
        writing.started = true;
        writing.delivered = writing.text;
        let held = core::mem::take(&mut writing.held);
        (!held.is_empty()).then_some(held)
    }

    /// Nothing more of the response `tag` is to be read, or written.
    pub(crate) fn stop(&mut self, tag: OpTag) {
        self.writing.take_if(|writing| writing.tag == tag);
    }

    /// Nothing more of anything is to be read.
    pub(crate) fn halt(&mut self) {
        self.writing = None;
    }

    /// What the kernel is to be told at once of the text it has just
    /// released: that it is ready, and then how much of it the reader
    /// has. Returns whether there was anything.
    pub(crate) fn settle(&mut self, events: &mut Vec<Event>) -> bool {
        let Some(writing) = &mut self.writing else {
            return false;
        };
        let before = events.len();
        let last = writing.finished;
        if writing.text > writing.counted || (last && !writing.ended) {
            writing.counted = writing.text;
            writing.ended = last;
            let ready = OpEvent::TtsSegment {
                text_end: writing.text,
                audio_end: MediaDur::ZERO,
                last,
            };
            events.push(Event::Op(writing.tag, ready));
        }
        // All of it is with the reader once the kernel has been told that
        // there is no more of it.
        let state = (
            writing.delivered,
            writing.ended && writing.delivered == writing.text,
        );
        if writing.started && writing.reported != Some(state) {
            writing.reported = Some(state);
            let read = OpEvent::Played {
                handed: MediaDur::ZERO,
                acked: MediaDur::ZERO,
                heard_text: state.0,
                drained: state.1,
            };
            events.push(Event::Op(writing.tag, read));
        }
        events.len() > before
    }

    fn of(&mut self, tag: OpTag) -> Option<&mut Writing> {
        self.writing.as_mut().filter(|writing| writing.tag == tag)
    }
}
