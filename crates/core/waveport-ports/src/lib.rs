//! Waveport ports.
//!
//! Stability: experimental
//!
//! This crate defines the boundary between the application and the outside
//! world: the traits adapters implement and the capability types they
//! report. It contains no implementations and depends only on the domain.
//!
//! # Three styles
//!
//! A port's style follows how often it is called.
//!
//! - **Provider streams** ([`stt`], [`llm`], [`tts`], [`transport`]) are
//!   split in two halves. The half that sends takes without waiting and
//!   says [`Full`](error::Full) when it has no room. The half that receives
//!   is polled, and it is the one that does the adapter's work: an adapter
//!   starts no task of its own, so dropping the receiving half is how a
//!   stream is cancelled.
//! - **Requests** ([`journal`], [`tool`]) return a boxed future. They
//!   happen a few times in a turn.
//! - **Models** ([`model`]) are plain functions over batches, called from
//!   the threads that own them.
//!
//! Nothing here names an async runtime, so an adapter is free in its
//! choice and the simulation can drive every port by hand.
//!
//! # Capabilities
//!
//! Providers differ in what they can do, and the differences decide how a
//! session is run. Each port says what its adapter can do in a `Caps`
//! type, and [`plan::plan`] turns those into a [`plan::SessionPlan`]
//! before the session starts, or says why it cannot.

pub mod audio;
pub mod error;
pub mod journal;
pub mod llm;
pub mod model;
pub mod plan;
pub mod stt;
pub mod telemetry;
pub mod tool;
pub mod transport;
pub mod tts;
