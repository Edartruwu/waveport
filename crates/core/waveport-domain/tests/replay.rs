//! Sessions kept as files.
//!
//! Each file in `traces/` is one session: the events its kernel was
//! given, a line each, and under every line what the kernel emitted for
//! it. The test gives a new kernel the same events and compares what it
//! emits with the file, so a change in what the kernel does shows up as
//! the first line that differs, whether or not any other test minds.
//!
//! When the change is meant, `WAVEPORT_BLESS=1 cargo nextest run -p
//! waveport-domain --test replay` writes the files anew, and the diff of
//! the files is the change to review.
//!
//! The sessions were made up by the seeded simulation. A file's name is
//! the profile and the seed that give its kernel's configuration.

use std::fs;
use std::path::PathBuf;

use waveport_kernel_sim::seeded::{Profile, record};
use waveport_kernel_sim::trace::{Trace, replay};

/// The sessions that are kept: one with four tool calls and their
/// continuation, one where the floor changes hands among adapters that
/// misbehave, one where another agent answers a call, one that is closed
/// with a call nobody can vouch for, one in which the process dies
/// twice, and one in which three people press keys as they speak and a
/// run of keys ends with what was said after it.
const KEPT: [(Profile, u64); 6] = [
    (Profile::Calm, 70),
    (Profile::Chaos, 0x1_0000_0123),
    (Profile::Shutdown, 0x4_0000_0002),
    (Profile::Shutdown, 0x4_0000_0059),
    (Profile::Crashes, 0x5_0000_0063),
    (Profile::Keys, 0x7_0000_03bb),
];

fn path(profile: Profile, seed: u64) -> PathBuf {
    let name = format!("{}-{seed}.trace", profile.name());
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/traces")
        .join(name)
}

fn header(profile: Profile, seed: u64) -> String {
    format!(
        "# Profile {}, seed {seed}. A line is an event the kernel was given, with the time in\n\
         # milliseconds; under it are the facts (=) and the commands (>) the kernel emitted.\n\
         # Written by `WAVEPORT_BLESS=1 cargo nextest run -p waveport-domain --test replay`.\n",
        profile.name()
    )
}

#[test]
fn the_kernel_does_what_it_did() {
    let bless = std::env::var_os("WAVEPORT_BLESS").is_some();
    for (profile, seed) in KEPT {
        let recorded = record(profile, seed).unwrap();
        let path = path(profile, seed);
        let trace = if bless {
            recorded.trace.clone()
        } else {
            let kept = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{} cannot be read: {error}", path.display()));
            Trace::parse(&kept).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        };
        let replayed = replay(&recorded.config, || recorded.policies(), &trace).unwrap();
        let now = header(profile, seed) + &replayed;
        if bless {
            fs::write(&path, now).unwrap();
            continue;
        }
        let kept = fs::read_to_string(&path).unwrap();
        let differs = kept
            .lines()
            .zip(now.lines())
            .position(|(was, is)| was != is)
            .or_else(|| (kept.lines().count() != now.lines().count()).then_some(0));
        if let Some(line) = differs {
            let was = kept.lines().nth(line).unwrap_or("");
            let is = now.lines().nth(line).unwrap_or("");
            panic!(
                "the kernel no longer does what {} records. Line {} was\n  {was}\nand is\n  \
                 {is}\nIf that is meant, write the files anew with WAVEPORT_BLESS=1 and review \
                 their diff.",
                path.display(),
                line + 1
            );
        }
    }
}
