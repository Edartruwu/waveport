//! A place for the command to stop at, for the test that kills it there.
//!
//! What a call is after its process died depends on where the process
//! was, and the places that matter are too brief to hit from outside:
//! between a record and the commit that stores it, or between what a
//! tool did and its answer. A test names such a place in the environment
//! variable `WAVEPORT_TEST_HALT`, and the command stops there: it says
//! `halted: <place>` and does nothing more of that kind until it is
//! killed.
//!
//! | Place | Where the command stops |
//! |---|---|
//! | `before:<kind>` | the journal, before it commits a record of this kind |
//! | `after:<kind>` | the journal, once it has committed a record of this kind |
//! | `before:state`, `after:state` | the same of a call's stored state |
//! | `tool` | the tool that takes notes, once it has written and before it answers |
//!
//! A kind is the `kind` of a journal record, such as `turn_committed`.
//! The journal writes on a thread of its own, so everything else goes
//! on while it is stopped: the call is where it would be if the commit
//! took that long. Nothing reads the variable but this, and a command
//! that is not told to stop anywhere does nothing here.

use core::time::Duration;
use std::io::Write as _;

use waveport_store_journal::{Probe, Seen};

/// The environment variable that names the place.
const VARIABLE: &str = "WAVEPORT_TEST_HALT";

/// Which side of a commit the journal stops on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    Before,
    After,
}

/// Where the command is to stop, if anywhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Halt {
    /// Nowhere.
    Nowhere,
    /// In the journal, at the commit that holds a record of this kind,
    /// or a state.
    Commit(Side, String),
    /// In the tool that takes notes, once it has written.
    Tool,
}

impl Halt {
    /// What the environment says.
    ///
    /// # Errors
    ///
    /// The variable names no place there is.
    pub(crate) fn from_env() -> Result<Self, String> {
        match std::env::var(VARIABLE) {
            Ok(place) => Self::read(&place)
                .ok_or_else(|| format!("{VARIABLE} names no place to stop at: {place:?}")),
            Err(_) => Ok(Self::Nowhere),
        }
    }

    fn read(place: &str) -> Option<Self> {
        if place == "tool" {
            return Some(Self::Tool);
        }
        let (side, kind) = place.split_once(':')?;
        let side = match side {
            "before" => Side::Before,
            "after" => Side::After,
            _ => return None,
        };
        let plain = |c: char| c.is_ascii_lowercase() || c == '_';
        (!kind.is_empty() && kind.chars().all(plain)).then(|| Self::Commit(side, kind.to_owned()))
    }

    /// What watches the journal's commits for the place, if the place is
    /// in the journal.
    pub(crate) fn probe(&self) -> Option<Box<dyn Probe>> {
        match self {
            Self::Commit(side, kind) => Some(Box::new(AtCommit {
                side: *side,
                kind: kind.clone(),
                tag: format!("\"kind\":\"{kind}\""),
            })),
            Self::Nowhere | Self::Tool => None,
        }
    }

    /// Whether the tool that takes notes is to stop once it has written.
    pub(crate) fn in_tool(&self) -> bool {
        *self == Self::Tool
    }
}

/// Says that the command has stopped at `place`, where whoever started
/// it reads it.
pub(crate) fn say(place: &str) {
    let mut out = std::io::stdout().lock();
    // If nobody reads it, nobody waits for it either.
    let _ = writeln!(out, "halted: {place}");
    let _ = out.flush();
}

/// Stops the thread it is called on, for as long as the process lives.
fn stop(place: &str) -> ! {
    say(place);
    loop {
        std::thread::sleep(Duration::from_secs(3_600));
    }
}

/// Stops the journal at the commit that holds what it looks for.
struct AtCommit {
    side: Side,
    kind: String,
    /// How a record of the kind says that it is one.
    tag: String,
}

impl AtCommit {
    fn holds(&self, seen: &Seen<'_>) -> bool {
        if self.kind == "state" {
            return seen.states > 0;
        }
        let tag = self.tag.as_bytes();
        seen.records
            .iter()
            .any(|record| record.windows(tag.len()).any(|window| window == tag))
    }
}

impl Probe for AtCommit {
    fn committing(&mut self, seen: &Seen<'_>) {
        if self.side == Side::Before && self.holds(seen) {
            stop(&format!("before:{}", self.kind));
        }
    }

    fn committed(&mut self, seen: &Seen<'_>, _took: Duration) {
        if self.side == Side::After && self.holds(seen) {
            stop(&format!("after:{}", self.kind));
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use waveport_store_journal::Seen;

    use super::{AtCommit, Halt, Side};

    #[test]
    fn a_place_is_read_from_how_it_is_named() {
        assert_eq!(Halt::read("tool"), Some(Halt::Tool));
        assert_eq!(
            Halt::read("before:tool_authorized"),
            Some(Halt::Commit(Side::Before, "tool_authorized".to_owned()))
        );
        assert_eq!(
            Halt::read("after:state"),
            Some(Halt::Commit(Side::After, "state".to_owned()))
        );
        for none in [
            "",
            "before",
            "before:",
            "during:state",
            "after:a\"b",
            "Tool",
        ] {
            assert_eq!(Halt::read(none), None, "{none:?}");
        }
        assert!(Halt::Tool.in_tool() && Halt::Tool.probe().is_none());
        assert!(Halt::Nowhere.probe().is_none() && !Halt::Nowhere.in_tool());
        assert!(Halt::read("after:state").unwrap().probe().is_some());
    }

    #[test]
    fn a_commit_is_the_place_when_it_holds_what_is_looked_for() {
        let at = |kind: &str| AtCommit {
            side: Side::Before,
            kind: kind.to_owned(),
            tag: format!("\"kind\":\"{kind}\""),
        };
        let records = [
            Bytes::from_static(br#"{"v":1,"seq":1,"fact":{"kind":"participant_joined","who":0}}"#),
            Bytes::from_static(
                br#"{"v":1,"seq":2,"fact":{"kind":"turn_committed","text":"a \"kind\":\"state\""}}"#,
            ),
        ];
        let seen = |states| Seen {
            requests: 1,
            records: &records,
            states,
        };
        assert!(at("turn_committed").holds(&seen(0)));
        assert!(!at("tool_authorized").holds(&seen(0)));
        // A state is not a record, whatever a record says.
        assert!(!at("state").holds(&seen(0)));
        assert!(at("state").holds(&seen(1)));
    }
}
