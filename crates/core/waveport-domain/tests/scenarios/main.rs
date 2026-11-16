//! The kernel scenarios, and where each one stands.
//!
//! Each scenario closes a race that conversational frameworks are known to
//! lose, and is explored along every path its world allows. The files in
//! `scenarios/` are written before the kernel does what they ask and are
//! not edited to make them pass: needing to change one is the sign that
//! the kernel's design is wrong.
//!
//! The first six were written before the kernel of phase 1, and all six
//! pass. The other ten were written before phase 2 (decision 0004 in
//! `docs/decisions`): three of them are about a session of text, which the
//! kernel can already hold, and seven are about what it cannot do yet, a
//! line that drops, the keys of a telephone, and turns whose words come
//! after them.
//!
//! This file says how far the kernel has got. A scenario is listed with
//! `unstarted` while it fails at its first expectation, with `pending`
//! while it gets further and still fails, and with `passes` once it holds
//! on every path. The listing is checked both ways, so a scenario that
//! starts to pass must be promoted here.

mod backchannel_tool;
mod hybrid_front_back;
mod keys_after_speech;
mod keys_are_one_turn;
mod late_words;
mod line_drops;
mod line_stays_gone;
mod lost_input;
mod shutdown;
mod sound_before_words;
mod spoken_text;
mod three_party;
mod typed_chat;
mod typed_cut_off;
mod typed_twice;
mod words_after_barge_in;

use waveport_kernel_sim::check::{Report, passes, pending, unstarted};

#[test]
fn lost_input() -> Result<(), Report> {
    passes(lost_input::lost_input)
}

#[test]
fn backchannel_tool() -> Result<(), Report> {
    passes(backchannel_tool::backchannel_tool)
}

#[test]
fn spoken_text() -> Result<(), Report> {
    passes(spoken_text::spoken_text)
}

#[test]
fn shutdown() -> Result<(), Report> {
    passes(shutdown::shutdown)
}

#[test]
fn hybrid_front_back() -> Result<(), Report> {
    passes(hybrid_front_back::hybrid_front_back)
}

#[test]
fn three_party() -> Result<(), Report> {
    passes(three_party::three_party)
}

#[test]
fn typed_chat() -> Result<(), Report> {
    passes(typed_chat::typed_chat)
}

#[test]
fn typed_twice() -> Result<(), Report> {
    passes(typed_twice::typed_twice)
}

#[test]
fn typed_cut_off() -> Result<(), Report> {
    passes(typed_cut_off::typed_cut_off)
}

/// Waits for the kernel to commit a turn whose words come later.
#[test]
fn late_words() -> Result<(), Report> {
    unstarted(late_words::late_words)
}

/// Waits for the same.
#[test]
fn sound_before_words() -> Result<(), Report> {
    unstarted(sound_before_words::sound_before_words)
}

/// Waits for the same.
#[test]
fn words_after_barge_in() -> Result<(), Report> {
    unstarted(words_after_barge_in::words_after_barge_in)
}

/// Waits for the kernel to act on a line that drops: today it is told and
/// goes on speaking.
#[test]
fn line_drops() -> Result<(), Report> {
    pending(line_drops::line_drops)
}

/// Waits for the same.
#[test]
fn line_stays_gone() -> Result<(), Report> {
    pending(line_stays_gone::line_stays_gone)
}

/// Waits for the kernel to take keys: today there is no event to tell it
/// of one.
#[test]
fn keys_are_one_turn() -> Result<(), Report> {
    pending(keys_are_one_turn::keys_are_one_turn)
}

/// Waits for the same.
#[test]
fn keys_after_speech() -> Result<(), Report> {
    pending(keys_after_speech::keys_after_speech)
}
