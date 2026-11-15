//! Running a scenario along every path its world allows.

use std::cell::RefCell;
use std::rc::Rc;

use crate::failure::{Failure, Verdict};
use crate::setup::Setup;
use crate::sim::Sim;

/// The most paths one scenario may have. Past this the scenario is asking
/// for more orders than a test should explore.
const MAX_PATHS: usize = 20_000;

/// One point where the world could go several ways.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Choice {
    taken: usize,
    of: usize,
}

/// The choices of one run, and through them the walk over all runs.
///
/// A run replays the recorded choices and takes the first way at any choice
/// beyond them. Between runs the last choice that has a way left moves to
/// its next way, and the choices after it are forgotten: a depth-first walk
/// of the tree of choices that needs no copy of the scenario's state.
#[derive(Debug, Default)]
pub(crate) struct Chooser {
    path: Vec<Choice>,
    cursor: usize,
    diverged: bool,
}

impl Chooser {
    /// Picks one of `of` ways, numbered from 0.
    pub(crate) fn choose(&mut self, of: usize) -> usize {
        if of <= 1 {
            return 0;
        }
        let taken = if let Some(choice) = self.path.get(self.cursor) {
            // A scenario must make the same choices in the same order when
            // it is replayed; otherwise the walk means nothing.
            self.diverged |= choice.of != of;
            choice.taken.min(of - 1)
        } else {
            self.path.push(Choice { taken: 0, of });
            0
        };
        self.cursor += 1;
        taken
    }

    /// The ways taken so far in this run.
    pub(crate) fn taken(&self) -> Vec<usize> {
        self.path
            .iter()
            .take(self.cursor)
            .map(|choice| choice.taken)
            .collect()
    }

    /// Moves to the next run. False when every path has been walked.
    fn next_run(&mut self) -> bool {
        self.path.truncate(self.cursor);
        self.cursor = 0;
        while let Some(last) = self.path.last_mut() {
            if last.taken + 1 < last.of {
                last.taken += 1;
                return true;
            }
            self.path.pop();
        }
        false
    }
}

/// What a scenario is given: the place where its sessions run and where its
/// choices are recorded.
#[derive(Debug)]
pub struct Lab {
    chooser: Rc<RefCell<Chooser>>,
}

impl Lab {
    /// A session set up as `setup` says, with one caller already in it.
    pub fn sim(&mut self, setup: Setup) -> Sim {
        Sim::new(setup, Rc::clone(&self.chooser))
    }

    /// A choice of the scenario's own: the scenario is run once with
    /// `false` here and once with `true`.
    pub fn either(&mut self) -> bool {
        self.chooser.borrow_mut().choose(2) == 1
    }
}

/// What an exploration covered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Explored {
    /// How many paths the scenario was run along.
    pub paths: usize,
}

/// Runs `scenario` along every path its world allows, stopping at the first
/// failure.
///
/// # Errors
///
/// The first failure, which names the path that led to it.
pub fn explore(scenario: impl Fn(&mut Lab) -> Verdict) -> Result<Explored, Box<Failure>> {
    let chooser = Rc::new(RefCell::new(Chooser::default()));
    let mut paths = 0;
    loop {
        let mut lab = Lab {
            chooser: Rc::clone(&chooser),
        };
        let verdict = scenario(&mut lab);
        paths += 1;
        let mut chooser = chooser.borrow_mut();
        let path = chooser.taken();
        verdict.map_err(|mut failure| {
            failure.path.clone_from(&path);
            failure
        })?;
        if chooser.diverged {
            return Err(Box::new(exploration_failed(
                "the scenario to make the same choices whenever it is replayed",
                "it made different ones, so it depends on something other than its choices",
                path,
            )));
        }
        if !chooser.next_run() {
            return Ok(Explored { paths });
        }
        if paths >= MAX_PATHS {
            return Err(Box::new(exploration_failed(
                &format!("at most {MAX_PATHS} paths"),
                "the scenario has more",
                path,
            )));
        }
    }
}

fn exploration_failed(expected: &str, found: &str, path: Vec<usize>) -> Failure {
    Failure {
        ordinal: 0,
        expected: expected.to_owned(),
        found: found.to_owned(),
        file: file!(),
        line: line!(),
        at_ms: 0,
        path,
        log: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::{Chooser, Explored, explore};

    #[test]
    fn every_combination_of_choices_is_walked_once() {
        let seen = RefCell::new(Vec::new());
        let explored = explore(|lab| {
            let first = lab.either();
            // The second choice only exists on one side of the first.
            let second = first && lab.either();
            let third = lab.either();
            seen.borrow_mut().push((first, second, third));
            Ok(())
        });
        assert_eq!(explored, Ok(Explored { paths: 6 }));
        assert_eq!(
            seen.into_inner(),
            [
                (false, false, false),
                (false, false, true),
                (true, false, false),
                (true, false, true),
                (true, true, false),
                (true, true, true),
            ]
        );
    }

    #[test]
    fn a_scenario_without_choices_runs_once() {
        assert_eq!(explore(|_| Ok(())), Ok(Explored { paths: 1 }));
    }

    #[test]
    fn a_single_way_is_not_a_choice() {
        let mut chooser = Chooser::default();
        assert_eq!(chooser.choose(1), 0);
        assert_eq!(chooser.choose(0), 0);
        assert_eq!(chooser.taken(), Vec::<usize>::new());
        assert!(!chooser.next_run());
    }

    #[test]
    fn a_scenario_that_changes_between_replays_is_refused() {
        let runs = RefCell::new(0);
        let failure = explore(|lab| {
            *runs.borrow_mut() += 1;
            let ways = if *runs.borrow() == 1 { 2 } else { 3 };
            lab.chooser.borrow_mut().choose(ways);
            Ok(())
        })
        .unwrap_err();
        assert_eq!(failure.ordinal, 0);
        assert!(failure.found.contains("different"), "{failure}");
    }
}
