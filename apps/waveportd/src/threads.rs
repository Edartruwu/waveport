//! Where the threads of a daemon run.
//!
//! Sessions run on the threads of the shards. Left alone, the system
//! moves every thread of the daemon among the processors as it sees fit.
//! A daemon whose file names the processors of its shards (`processors`)
//! keeps the thread of each shard on the one named for it, and every
//! other thread off all of them.
//!
//! A thread begins on the processors of the thread that started it. So
//! the first thread of the process is placed before it starts any other,
//! which is what [`Threads::place`] does. The threads that serve
//! connections, the journal's, those of the models that listen and those
//! of telemetry then begin off the processors of the shards, because
//! whoever started them was.

use waveport_assembly::pinning::{Pinning, listed};
use waveport_assembly::{Context as _, Failure};

use crate::Config;

/// Where the threads of a daemon run: wherever the system has them, or
/// the shards on processors of their own and every other thread off
/// those.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Threads {
    pinning: Option<Pinning>,
}

impl Threads {
    /// Places the threads of a daemon as `config` says, beginning with
    /// the thread this is called on. That is to be the thread every
    /// other thread of the daemon is started from, and it is to have
    /// started none yet.
    ///
    /// With no processors named, nothing is placed.
    ///
    /// # Errors
    ///
    /// `config` names processors for the shards, and they cannot be
    /// theirs: the system is not Linux, the process may not run on one
    /// of them, or none would be left for its other threads.
    pub fn place(config: &Config) -> Result<Self, Failure> {
        if config.processors.is_empty() {
            return Ok(Self { pinning: None });
        }
        let cannot = || "the shards cannot have the processors that `processors` names".to_owned();
        let pinning = Pinning::here(&config.processors).with(cannot)?;
        pinning.keep_off().with(cannot)?;
        Ok(Self {
            pinning: Some(pinning),
        })
    }

    /// Whether these are the threads of a daemon that is told `config`.
    pub(crate) fn are_of(&self, config: &Config) -> bool {
        let shards = self.pinning.as_ref().map_or(&[][..], Pinning::shards);
        shards == config.processors
    }

    /// The processors of the shards and of the other threads, if they
    /// are placed.
    pub(crate) fn pinning(&self) -> Option<&Pinning> {
        self.pinning.as_ref()
    }

    /// What whoever runs the daemon is told of where its threads are.
    pub(crate) fn note(&self) -> Option<String> {
        let pinning = self.pinning.as_ref()?;
        let shards: Vec<String> = pinning.shards().iter().map(usize::to_string).collect();
        Some(format!(
            "the shards are kept on processors {}, one each, and every other thread on {}",
            shards.join(", "),
            listed(pinning.rest())
        ))
    }
}

#[cfg(test)]
mod tests {
    use waveport_assembly::Failure;

    use super::Threads;
    use crate::{Config, Daemon};

    #[tokio::test]
    async fn a_daemon_is_not_started_with_threads_that_were_placed_for_another() {
        // Nothing is placed for a daemon that names no processors,
        // wherever that is asked.
        let plain = Config::from_toml("[agents.desk]\n").unwrap();
        let left_alone = Threads::place(&plain).unwrap();
        assert_eq!(left_alone.pinning(), None);
        assert_eq!(left_alone.note(), None);
        assert!(left_alone.are_of(&plain));
        // A daemon that names some is not started as if it had none:
        // its other threads would be on the processors of its shards.
        let placed = Config::from_toml("processors = [0]\n[agents.desk]\n").unwrap();
        assert!(!left_alone.are_of(&placed));
        let Err(Failure(why)) = Daemon::start(&placed, &left_alone).await.map(|_| ()) else {
            panic!("it was started");
        };
        assert!(
            why.contains("were not placed as `processors` says"),
            "{why}"
        );
    }
}
