//! Throwaway code for the phase 1 density spike. Not part of the product.
//!
//! The spike answers three questions before any product code depends on the
//! answers: which inference backend runs the two models, which allocator the
//! binaries use, and how many voice sessions one core carries.

pub mod alloc;
pub mod audio;
pub mod backend;
pub mod mel;
pub mod stats;

/// The error type of the spike: a message is all a throwaway needs.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Pins the calling thread to one logical CPU.
pub fn pin_to(cpu: usize) -> Result<()> {
    if core_affinity::set_for_current(core_affinity::CoreId { id: cpu }) {
        Ok(())
    } else {
        Err(format!("could not pin the thread to CPU {cpu}").into())
    }
}

/// Reads `--name value` style options: `flag("--sessions")`.
pub struct Options {
    args: Vec<String>,
}

impl Options {
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            args: std::env::args().skip(1).collect(),
        }
    }

    /// The value after `name`, if the option was given.
    #[must_use]
    pub fn value(&self, name: &str) -> Option<&str> {
        let at = self.args.iter().position(|arg| arg == name)?;
        self.args.get(at + 1).map(String::as_str)
    }

    /// The value after `name` parsed, or `default`.
    pub fn parse<T: std::str::FromStr>(&self, name: &str, default: T) -> Result<T> {
        match self.value(name) {
            None => Ok(default),
            Some(text) => text
                .parse()
                .map_err(|_| format!("invalid value for {name}: {text}").into()),
        }
    }

    /// Whether the bare flag `name` was given.
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.args.iter().any(|arg| arg == name)
    }
}
