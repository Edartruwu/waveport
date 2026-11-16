//! Waveport kernel scenarios.
//!
//! Stability: experimental
//!
//! A scenario is a short story about one session: who says what, what the
//! agent is going to answer, and what must be true afterwards. This crate
//! runs such a story against the real session kernel, with a simulated
//! world in place of every adapter: a listener that reports speech, a
//! transcriber, the turn-taking models, a scripted language model, speech
//! synthesis, playback, tools, the journal and the timers.
//!
//! ```
//! use waveport_kernel_sim::prelude::*;
//!
//! fn greeting(lab: &mut Lab) -> Verdict {
//!     let mut sim = lab.sim(Setup::new());
//!     let caller = sim.caller();
//!
//!     sim.agent_will(say("Hello, how can I help?"));
//!     sim.says(caller, "hello");
//!     let turn = sim.expect_turn(caller, "hello")?;
//!     let reply = sim.expect_reply(turn)?;
//!     sim.expect_spoken(reply, "Hello, how can I help?", Complete)?;
//!     sim.finish()
//! }
//!
//! // `explore` runs the story along every path its world allows.
//! let outcome = waveport_kernel_sim::explore(greeting);
//! # let _ = outcome;
//! ```
//!
//! Time is virtual and nothing is random. Where the world could go more than
//! one way, the scenario says so, with [`Lab::either`] for a choice of its
//! own and [`Sim::race`] for events that may arrive in any order, and
//! [`explore`] runs the story once for every combination. A failure names
//! the path that led to it, the expectation that did not hold and the last
//! steps of the session.
//!
//! The simulated world keeps fixed time: a word takes 300 ms to say, a
//! final transcript follows speech by 200 ms, and so on. The figures are in
//! the `world` module and are part of what scenarios rely on.
//!
//! Two more modules look at the kernel from other sides:
//!
//! - [`invariants`] holds a monitor that checks the kernel's invariants
//!   after every step, from the outside. Every scenario runs under it.
//! - [`seeded`] makes up sessions from a seed, with adapters that are late,
//!   repeat themselves, fall silent or send nonsense, and runs them under
//!   the same monitor.

pub mod check;
mod expect;
mod explore;
mod failure;
pub mod invariants;
mod script;
pub mod seeded;
mod setup;
mod sim;
mod world;

pub use expect::{Ending, Line, cut_off, spoke, user};
pub use explore::{Explored, Lab, explore};
pub use failure::{Failure, Verdict};
pub use script::{Script, say};
pub use setup::Setup;
pub use sim::{Call, Person, Reply, Sim, Turn};
pub use world::Timer;

/// Everything a scenario file needs.
pub mod prelude {
    pub use waveport_domain::config::Effect;
    pub use waveport_domain::fact::{Dismissal, Lifecycle};

    pub use crate::Ending::{Complete, Truncated};
    pub use crate::{
        Call, Lab, Line, Person, Reply, Script, Setup, Sim, Timer, Turn, Verdict, cut_off, say,
        spoke, user,
    };
}
