//! What the gates hold the code to.
//!
//! The numbers are in `perf/targets.toml`, at the root of the
//! repository, where a person can read them and a change to one is a
//! line in a pull request. The tests that are gates read them from
//! there, and so does `cargo xtask perf`.

use std::path::PathBuf;

/// The file the numbers are in.
#[must_use]
pub fn file() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../perf/targets.toml")
}

/// The numbers of `perf/targets.toml`.
#[derive(Clone, Debug)]
pub struct Targets(toml::Table);

impl Targets {
    /// Reads the file.
    ///
    /// # Panics
    ///
    /// The file is not there or cannot be read as TOML: a gate that
    /// does not know what to hold the code to cannot pass.
    #[must_use]
    pub fn load() -> Self {
        let file = file();
        #[expect(
            clippy::panic,
            reason = "a gate that does not know what to hold the code to cannot pass"
        )]
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|error| panic!("{} cannot be read: {error}", file.display()));
        Self::read(&text)
    }

    /// Reads the numbers from `text`.
    ///
    /// # Panics
    ///
    /// `text` is not TOML.
    #[must_use]
    pub fn read(text: &str) -> Self {
        #[expect(
            clippy::panic,
            reason = "a gate that does not know what to hold the code to cannot pass"
        )]
        let table = toml::from_str(text).unwrap_or_else(|error| panic!("not TOML: {error}"));
        Self(table)
    }

    /// The number at `path`, whose parts are joined by dots:
    /// `latency.answering.held_p99`.
    ///
    /// # Panics
    ///
    /// There is no number there.
    #[must_use]
    pub fn number(&self, path: &str) -> f64 {
        let mut parts = path.split('.');
        let mut value = parts.next().and_then(|first| self.0.get(first));
        for part in parts {
            value = value.and_then(|value| value.get(part));
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "the numbers of a target are small"
        )]
        let number = value.and_then(|value| {
            value
                .as_float()
                .or_else(|| value.as_integer().map(|whole| whole as f64))
        });
        #[expect(
            clippy::panic,
            reason = "a gate that does not know what to hold the code to cannot pass"
        )]
        number.unwrap_or_else(|| panic!("perf/targets.toml has no number at {path}"))
    }
}

#[cfg(test)]
mod tests {
    use super::Targets;

    #[test]
    fn a_number_is_found_by_its_path_whole_or_not() {
        let targets = Targets::read("[a.b]\nc = 1.5\nd = 3\n");
        assert!((targets.number("a.b.c") - 1.5).abs() < f64::EPSILON);
        assert!((targets.number("a.b.d") - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    #[should_panic(expected = "no number at a.b.x")]
    fn a_number_that_is_not_there_stops_the_gate() {
        let _ = Targets::read("[a.b]\nc = 1.5\n").number("a.b.x");
    }

    #[test]
    fn every_held_number_is_within_its_target() {
        // What the code is held to is never looser than what the design
        // asks for, where the design asks for anything.
        let targets = Targets::load();
        for part in ["answering", "phrasing", "sending", "ducking", "stopping"] {
            for of in ["p50", "p99"] {
                let held = targets.number(&format!("latency.{part}.held_{of}_ms"));
                let target = targets.number(&format!("latency.{part}.target_{of}_ms"));
                assert!(
                    held <= target,
                    "latency.{part}: {held} ms is held, {target} ms asked"
                );
            }
        }
        for kind in ["idle", "active"] {
            for line in ["pcm", "opus"] {
                let held = targets.number(&format!("memory.{kind}.held_{line}_kb"));
                let limit = targets.number(&format!("memory.{kind}.target_p99_kb"));
                assert!(
                    held <= limit,
                    "memory.{kind}: {held} KB is held, {limit} KB asked"
                );
            }
        }
    }
}
