//! Waveport control plane.
//!
//! Stability: experimental
//!
//! This crate runs the fleet: it registers nodes, offers jobs, places
//! sessions, grants leases, and drains and migrates sessions between nodes.
//! It depends on the domain and the ports, never on an adapter.
