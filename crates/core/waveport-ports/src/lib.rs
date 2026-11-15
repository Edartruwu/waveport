//! Waveport ports.
//!
//! Stability: experimental
//!
//! This crate defines the boundary between the application and the outside
//! world: the traits adapters implement (speech-to-text, LLM, text-to-speech,
//! transports, journal, bus and others) and the capability types they report.
//! It contains no implementations and depends only on the domain.
