//! The client protocol: what a client and a session say to each other.
//!
//! A version is a module, and a version that is published is not
//! changed in a way a reader of it would misread. [`v1`] is the first.

pub mod v1;
