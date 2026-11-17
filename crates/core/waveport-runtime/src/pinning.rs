//! Which processors the threads of a process are kept on.
//!
//! A shard's thread can be kept on one processor, and then that
//! processor is no other thread's: every other thread of the process is
//! kept on the ones that are left. That is what a [`Pinning`] says.
//!
//! Nothing here chooses a processor. Whoever runs the process names one
//! for each shard, because only they know which cores are fast, which
//! are two threads of one core, and what else runs on the machine
//! (decision 0003). A processor is named by the number the system has
//! for it, as `taskset -c` takes it.
//!
//! This is asked of Linux alone, which has a call that keeps a thread on
//! a set of processors. Anywhere else [`Pinning::here`] says that it
//! cannot be done, and there is no other way to make a [`Pinning`]: a
//! runtime is never told to keep its shards somewhere and then does not.
//!
//! A thread begins on the processors of the thread that started it. So
//! the threads of a process are placed in an order:
//!
//! 1. The thread that starts the others makes the [`Pinning`], while it
//!    may still run wherever the process may.
//! 2. It keeps itself off the shards' processors
//!    ([`Pinning::keep_off`]), before it starts any thread. Every thread
//!    it starts from then on, and every thread those start, is off them
//!    too.
//! 3. It hands the [`Pinning`] to the runtime, whose shards each take
//!    the one processor that is theirs.

use core::fmt;
use core::fmt::Write as _;

/// Which processors the threads of a process run on: one for each shard,
/// and the rest for every other thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pinning {
    shards: Vec<usize>,
    rest: Vec<usize>,
}

impl Pinning {
    /// Gives each shard the processor named for it in `shards`, and
    /// every other thread the processors that are left, of the ones the
    /// thread this is called on may run on.
    ///
    /// Which those are is asked of the system, so what a container was
    /// given, or `taskset`, is all there is to give. It is to be called
    /// before the thread is kept anywhere itself.
    ///
    /// # Errors
    ///
    /// The system has no such call, or did not answer. No processor was
    /// named, one was named twice, one is not the process's to run on,
    /// or none is left for the other threads.
    pub fn here(shards: &[usize]) -> Result<Self, PinError> {
        Self::among(shards, &allowed()?)
    }

    /// As [`Self::here`], of the processors `allowed`.
    fn among(shards: &[usize], allowed: &[usize]) -> Result<Self, PinError> {
        if shards.is_empty() {
            return Err(PinError::Nothing);
        }
        let mut given = Vec::with_capacity(shards.len());
        for &processor in shards {
            if given.contains(&processor) {
                return Err(PinError::Twice(processor));
            }
            if !allowed.contains(&processor) {
                return Err(PinError::NotAllowed {
                    processor,
                    allowed: allowed.to_vec(),
                });
            }
            given.push(processor);
        }
        let rest: Vec<usize> = allowed
            .iter()
            .copied()
            .filter(|processor| !given.contains(processor))
            .collect();
        if rest.is_empty() {
            return Err(PinError::NothingLeft);
        }
        Ok(Self {
            shards: given,
            rest,
        })
    }

    /// The processors of the shards, in the order of the shards.
    #[must_use]
    pub fn shards(&self) -> &[usize] {
        &self.shards
    }

    /// The processors of every other thread, in order.
    #[must_use]
    pub fn rest(&self) -> &[usize] {
        &self.rest
    }

    /// Keeps the thread this is called on off the processors of the
    /// shards, and with it every thread it starts from now on.
    ///
    /// # Errors
    ///
    /// The system did not do it.
    pub fn keep_off(&self) -> Result<(), PinError> {
        keep(&self.rest)
    }
}

/// Why threads are not kept on processors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PinError {
    /// The system has no call that keeps a thread on a set of
    /// processors.
    Unsupported,
    /// No processor was named.
    Nothing,
    /// There is not one processor for each shard.
    Count {
        /// How many shards there are to be.
        shards: usize,
        /// How many processors were named.
        processors: usize,
    },
    /// A processor was named for two shards.
    Twice(usize),
    /// A processor is not one the process may run on.
    NotAllowed {
        /// The processor that was named.
        processor: usize,
        /// The ones the process may run on.
        allowed: Vec<usize>,
    },
    /// Every processor the process may run on was named for a shard.
    NothingLeft,
    /// The system did not say which processors the thread may run on.
    Unknown(String),
    /// The system did not keep a thread on the processors it was given.
    Refused {
        /// The processors the thread was to be kept on.
        processors: Vec<usize>,
        /// What the system said.
        error: String,
    },
}

impl fmt::Display for PinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => f.write_str("a thread can be kept on a processor on Linux only"),
            Self::Nothing => f.write_str("no processor was named for a shard"),
            Self::Count { shards, processors } => write!(
                f,
                "{processors} processors were named for {shards} shards: each shard has one"
            ),
            Self::Twice(processor) => {
                write!(f, "processor {processor} was named for two shards")
            }
            Self::NotAllowed { processor, allowed } => write!(
                f,
                "processor {processor} is not one this process may run on, which are {}",
                listed(allowed)
            ),
            Self::NothingLeft => f.write_str(
                "every processor this process may run on was named for a shard, and its other \
                 threads need one too",
            ),
            Self::Unknown(error) => write!(
                f,
                "the system did not say which processors this process may run on: {error}"
            ),
            Self::Refused { processors, error } => write!(
                f,
                "a thread could not be kept on {}: {error}",
                listed(processors)
            ),
        }
    }
}

impl core::error::Error for PinError {}

/// Processors as the system lists them: `0-3,6`.
#[must_use]
pub fn listed(processors: &[usize]) -> String {
    let mut written = String::new();
    let mut rest = processors.iter().copied().peekable();
    while let Some(first) = rest.next() {
        let mut last = first;
        while let Some(next) = rest.next_if(|&next| last.checked_add(1) == Some(next)) {
            last = next;
        }
        if !written.is_empty() {
            written.push(',');
        }
        let _ = if last == first {
            write!(written, "{first}")
        } else {
            write!(written, "{first}-{last}")
        };
    }
    written
}

/// The processors the thread this is called on may run on, in order.
///
/// # Errors
///
/// The system has no such call, or did not answer.
pub fn allowed() -> Result<Vec<usize>, PinError> {
    sys::allowed()
}

/// Keeps the thread this is called on on `processors`.
pub(crate) fn keep(processors: &[usize]) -> Result<(), PinError> {
    sys::keep(processors)
}

#[cfg(target_os = "linux")]
mod sys {
    use rustix::thread::{CpuSet, sched_getaffinity, sched_setaffinity};

    use super::PinError;

    pub(super) fn allowed() -> Result<Vec<usize>, PinError> {
        // A machine with more processors than a set holds is answered
        // with an error, and not with the first of them.
        let set = sched_getaffinity(None).map_err(|error| PinError::Unknown(error.to_string()))?;
        Ok((0..CpuSet::MAX_CPU)
            .filter(|&processor| set.is_set(processor))
            .collect())
    }

    pub(super) fn keep(processors: &[usize]) -> Result<(), PinError> {
        let refused = |error: String| PinError::Refused {
            processors: processors.to_vec(),
            error,
        };
        let mut set = CpuSet::new();
        for &processor in processors {
            // A set has no room for it, and says so by stopping the
            // thread.
            if processor >= CpuSet::MAX_CPU {
                return Err(refused(format!(
                    "a processor past {} cannot be named",
                    CpuSet::MAX_CPU - 1
                )));
            }
            set.set(processor);
        }
        sched_setaffinity(None, &set).map_err(|error| refused(error.to_string()))
    }
}

#[cfg(not(target_os = "linux"))]
mod sys {
    use super::PinError;

    pub(super) const fn allowed() -> Result<Vec<usize>, PinError> {
        Err(PinError::Unsupported)
    }

    pub(super) const fn keep(_processors: &[usize]) -> Result<(), PinError> {
        Err(PinError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::{PinError, Pinning, listed};

    #[test]
    fn each_shard_has_a_processor_and_the_others_have_the_rest() {
        let pinning = Pinning::among(&[6, 2], &[0, 1, 2, 3, 6, 7]).unwrap();
        // In the order of the shards, and not of the processors.
        assert_eq!(pinning.shards(), [6, 2]);
        assert_eq!(pinning.rest(), [0, 1, 3, 7]);
    }

    #[test]
    fn what_cannot_be_given_is_said() {
        let allowed = [0, 1, 4, 5];
        let refused = |shards: &[usize]| Pinning::among(shards, &allowed).unwrap_err();
        assert_eq!(refused(&[]), PinError::Nothing);
        assert_eq!(refused(&[4, 1, 4]), PinError::Twice(4));
        // One the system has, and the process was not given.
        let not_allowed = PinError::NotAllowed {
            processor: 2,
            allowed: allowed.to_vec(),
        };
        assert_eq!(refused(&[1, 2]), not_allowed);
        assert_eq!(
            not_allowed.to_string(),
            "processor 2 is not one this process may run on, which are 0-1,4-5"
        );
        // The other threads of the process have to run somewhere.
        assert_eq!(refused(&[0, 1, 4, 5]), PinError::NothingLeft);
        assert!(Pinning::among(&[0, 1, 4], &allowed).is_ok());
        assert_eq!(Pinning::among(&[0], &[0]), Err(PinError::NothingLeft));
    }

    #[test]
    fn processors_are_listed_as_the_system_lists_them() {
        assert_eq!(listed(&[]), "");
        assert_eq!(listed(&[3]), "3");
        assert_eq!(listed(&[0, 1, 2, 3]), "0-3");
        assert_eq!(listed(&[0, 1, 4, 6, 7, 8, 11]), "0-1,4,6-8,11");
        assert_eq!(listed(&[usize::MAX - 1, usize::MAX]), {
            let (first, last) = (usize::MAX - 1, usize::MAX);
            format!("{first}-{last}")
        });
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn nothing_is_pinned_where_there_is_no_call_for_it() {
        assert_eq!(super::allowed(), Err(PinError::Unsupported));
        assert_eq!(Pinning::here(&[0]), Err(PinError::Unsupported));
        assert_eq!(super::keep(&[0]), Err(PinError::Unsupported));
    }

    #[cfg(target_os = "linux")]
    mod on_linux {
        use super::super::{PinError, Pinning, allowed, keep};

        /// Runs `work` on a thread of its own, so that where a test
        /// keeps a thread is not where the next test begins.
        fn on_a_thread<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
            std::thread::spawn(work).join().unwrap()
        }

        #[test]
        fn a_thread_is_kept_where_it_is_told_and_the_ones_it_starts_begin_there() {
            let all = allowed().unwrap();
            assert_ne!(all, [] as [usize; 0]);
            let Some((&shard, others)) = all.split_last().filter(|(_, others)| !others.is_empty())
            else {
                // A machine with one processor has none to give away.
                assert_eq!(Pinning::here(&all), Err(PinError::NothingLeft));
                return;
            };
            let pinning = Pinning::here(&[shard]).unwrap();
            assert_eq!(pinning.shards(), [shard]);
            assert_eq!(pinning.rest(), others);
            let rest = others.to_vec();
            let (here, started, on_one) = on_a_thread(move || {
                pinning.keep_off().unwrap();
                let here = allowed().unwrap();
                // A thread begins where its maker is.
                let started = on_a_thread(|| allowed().unwrap());
                // And one that is kept elsewhere leaves its maker alone.
                let on_one = on_a_thread(move || {
                    keep(&[shard]).unwrap();
                    allowed().unwrap()
                });
                assert_eq!(allowed().unwrap(), here);
                (here, started, on_one)
            });
            assert_eq!(here, rest);
            assert_eq!(started, rest);
            assert_eq!(on_one, [shard]);
            // The thread of the test is where it was.
            assert_eq!(allowed().unwrap(), all);
        }

        #[test]
        fn a_processor_the_process_may_not_run_on_is_refused() {
            let all = allowed().unwrap();
            let Some(missing) = (0..1024).find(|processor| !all.contains(processor)) else {
                return;
            };
            assert_eq!(
                Pinning::here(&[missing]),
                Err(PinError::NotAllowed {
                    processor: missing,
                    allowed: all.clone(),
                })
            );
            // One the machine does not have, asked of the system itself.
            let Some(nowhere) = (0..1024).rev().find(|processor| !all.contains(processor)) else {
                return;
            };
            let kept = on_a_thread(move || (keep(&[nowhere]), allowed().unwrap()));
            if let (Err(refused), still) = kept {
                assert!(matches!(refused, PinError::Refused { .. }), "{refused}");
                assert_eq!(still, all);
            }
            // No set has room for this one.
            let past = on_a_thread(|| keep(&[1 << 20]));
            let said = past.unwrap_err().to_string();
            assert!(
                said.contains("a processor past 1023 cannot be named"),
                "{said}"
            );
            // And a thread cannot be kept nowhere.
            let nothing = on_a_thread(|| keep(&[]));
            assert!(
                matches!(nothing, Err(PinError::Refused { .. })),
                "{nothing:?}"
            );
        }
    }
}
