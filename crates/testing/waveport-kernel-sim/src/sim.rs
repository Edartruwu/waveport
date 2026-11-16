//! One session: the kernel, the world around it and the clock.

use core::panic::Location;
use std::cell::RefCell;
use std::rc::Rc;

use waveport_domain::command::Command;
use waveport_domain::event::{CloseReason, ControlInput, Event};
use waveport_domain::fact::Fact;
use waveport_domain::ids::{OpTag, ParticipantId, ToolCallId, TurnId};
use waveport_domain::kernel::Kernel;
use waveport_domain::out::Out;
use waveport_domain::policy::BoxedPolicies;
use waveport_domain::time::MonoTime;

use crate::explore::Chooser;
use crate::failure::{Failure, Verdict};
use crate::script::Script;
use crate::setup::{Names, Setup};
use crate::world::{Arrival, Manner, Ms, Timer, World};

/// How long an expectation waits, in virtual time, before it gives up.
pub(crate) const PATIENCE_MS: Ms = 20_000;
/// How many steps of the session a failure shows.
const LOG_TAIL: usize = 40;
/// More steps than any scenario needs. Past this the kernel and the world
/// are feeding each other without the clock moving.
const MAX_STEPS: usize = 200_000;

/// A human in the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Person(pub(crate) ParticipantId);

/// A committed user turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Turn(pub(crate) TurnId);

/// A response of an agent: a reply to a turn, a continuation after tool
/// results, or a back agent's work on a delegated call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reply(pub(crate) OpTag);

/// A tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Call(pub(crate) ToolCallId);

/// Something the kernel emitted: a command or a fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum What {
    Cmd(Command),
    Fact(Fact),
}

/// Something the kernel emitted, kept for the expectations to look at.
#[derive(Clone, Debug)]
pub(crate) struct Seen {
    pub(crate) what: What,
    /// The text it carried, if any.
    pub(crate) text: String,
    /// Whether an expectation has already been satisfied by it. Each
    /// emission satisfies one expectation, which is how a scenario states
    /// that something happens once.
    pub(crate) claimed: bool,
}

/// What looking for something found.
pub(crate) enum Probe<T> {
    /// It has happened.
    Found(T),
    /// Something else happened that rules it out.
    Wrong(String),
    /// Nothing yet.
    NotYet,
}

/// One session, driven by a scenario.
///
/// The methods fall in three groups. Some make things happen in the world:
/// [`says`](Self::says), [`agent_will`](Self::agent_will),
/// [`close`](Self::close). Some shape how the world behaves:
/// [`race`](Self::race), [`tools_wait`](Self::tools_wait),
/// [`adapters_go_silent`](Self::adapters_go_silent). The rest, named
/// `expect_…`, let virtual time pass until something has happened and fail
/// if it does not.
#[derive(Debug)]
pub struct Sim {
    kernel: Kernel<BoxedPolicies>,
    out: Out,
    pub(crate) now: Ms,
    pub(crate) world: World,
    chooser: Rc<RefCell<Chooser>>,
    pub(crate) seen: Vec<Seen>,
    log: Vec<String>,
    /// How many checks have run.
    checks: u32,
    steps: usize,
    /// The sequence number of the last fact.
    last_seq: u64,
    pub(crate) closed_at: Option<Ms>,
    /// Calls the scenario said must never be cancelled.
    pub(crate) protected: Vec<Call>,
}

impl Sim {
    pub(crate) fn new(setup: Setup, chooser: Rc<RefCell<Chooser>>) -> Self {
        let built = setup.build().and_then(|(config, policies, names)| {
            let kernel = Kernel::new(config, policies).map_err(|error| error.to_string())?;
            Ok((kernel, names))
        });
        let (kernel, names, fault) = match built {
            Ok((kernel, names)) => (kernel, names, None),
            Err(fault) => (Kernel::default(), Names::fallback(), Some(fault)),
        };
        let mut sim = Self {
            kernel,
            out: Out::new(),
            now: 0,
            world: World::new(names),
            chooser,
            seen: Vec::new(),
            log: Vec::new(),
            checks: 0,
            steps: 0,
            last_seq: 0,
            closed_at: None,
            protected: Vec::new(),
        };
        if let Some(fault) = fault {
            sim.world.blame(fault);
        }
        sim.step(&Event::Control(ControlInput::Join {
            who: ParticipantId(1),
        }));
        sim
    }

    // ---- People ----------------------------------------------------------

    /// The human who is in the session from the start.
    #[must_use]
    pub fn caller(&self) -> Person {
        Person(ParticipantId(1))
    }

    /// Another human joins.
    pub fn joins(&mut self, name: &str) -> Person {
        let who = self.world.names.add_person(name);
        self.deliver(&Event::Control(ControlInput::Join { who }));
        Person(who)
    }

    /// `who` starts to say `text`, at 300 ms a word, meaning it. The start
    /// of their speech is reported at once; its end and its transcript
    /// follow as time passes. Said over the agent's speech, it is a
    /// barge-in.
    pub fn says(&mut self, who: Person, text: &str) {
        self.utter(who, text, Manner::Speech);
    }

    /// `who` makes a listener's noise, such as "mm-hm". Made over the
    /// agent's speech, it is a backchannel.
    pub fn murmurs(&mut self, who: Person, text: &str) {
        self.utter(who, text, Manner::Murmur);
    }

    fn utter(&mut self, who: Person, text: &str, manner: Manner) {
        if let Some(started) = self.world.utter(self.now, who.0, text, manner) {
            self.deliver(&started);
        }
    }

    /// Lets this much time pass.
    pub fn wait_ms(&mut self, millis: u64) {
        self.run_for(millis);
    }

    // ---- Agents and tools --------------------------------------------------

    /// The next response of the agent that talks to the caller. Responses
    /// are used in the order they are given, and a response nobody scripted
    /// is a failure.
    pub fn agent_will(&mut self, script: Script) {
        let front = self.kernel.config().front;
        self.world.script(front, script);
    }

    /// The next response of the back agent `name`.
    pub fn back_agent_will(&mut self, name: &str, script: Script) {
        match self.world.names.agent_id(name) {
            Some(agent) => self.world.script(agent, script),
            None => self.world.blame(format!(
                "the scenario scripts `{name}`, which is not an agent of the setup"
            )),
        }
    }

    /// From now on a tool returns only when the scenario says what, with
    /// [`tool_returns`](Self::tool_returns). Otherwise every tool returns
    /// `{"ok":true}` after 300 ms.
    pub fn tools_wait(&mut self) {
        self.world.hold_tools();
    }

    /// `call` returns `result`, which reaches the session 300 ms from now.
    /// Saying it twice delivers the result twice.
    pub fn tool_returns(&mut self, call: Call, result: &str) {
        self.world.tool_returns(self.now, call.0, result);
    }

    // ---- The world ---------------------------------------------------------

    /// Everything that perception, the tools and the journal have on its
    /// way arrives now, in every order that keeps each source's own order.
    /// So does whatever they send in answer to the kernel meanwhile. The
    /// scenario is run once for each order.
    ///
    /// Events from the language model, speech synthesis and playback are
    /// not part of the race, and neither are timers: they arrive when they
    /// are due.
    ///
    /// # Errors
    ///
    /// Something happened during the race that must never happen, such as a
    /// response nobody scripted.
    #[track_caller]
    pub fn race(&mut self) -> Verdict {
        loop {
            let chooser = Rc::clone(&self.chooser);
            let next = self
                .world
                .pop_racing(|ways| chooser.borrow_mut().choose(ways));
            match next {
                Some(pending) => self.arrive(pending.arrival),
                None => break,
            }
            if self.world.fault().is_some() {
                break;
            }
        }
        let outcome = self
            .world
            .fault()
            .map_or(Ok(()), |fault| Err(fault.to_owned()));
        self.settle(
            "the race to run without anything forbidden happening",
            outcome,
        )
    }

    /// From now on firings of `timer` never arrive.
    pub fn lose_timer(&mut self, timer: Timer) {
        self.world.lose(timer);
    }

    /// From now on no adapter says anything: not the listener, the
    /// transcriber, the models, synthesis, playback, the tools or the
    /// journal. What they had on its way is lost. Timers still fire.
    pub fn adapters_go_silent(&mut self) {
        self.world.go_silent();
        self.note("-- every adapter goes silent");
    }

    /// The application asks the session to close.
    pub fn close(&mut self) {
        self.closed_at = Some(self.now);
        self.deliver(&Event::Control(ControlInput::Close {
            reason: CloseReason::Requested,
        }));
    }

    // ---- Machinery ---------------------------------------------------------

    pub(crate) fn kernel(&self) -> &Kernel<BoxedPolicies> {
        &self.kernel
    }

    fn note(&mut self, line: &str) {
        self.log.push(format!("{:>7} ms  {line}", self.now));
    }

    /// Gives the kernel one event and has the world react to what the
    /// kernel asks for.
    fn step(&mut self, event: &Event) {
        self.steps += 1;
        if self.steps == MAX_STEPS {
            self.world.blame(format!(
                "the session took {MAX_STEPS} steps without coming to rest"
            ));
        }
        self.kernel
            .step(MonoTime::from_millis(self.now), event, &mut self.out);
        self.note(&format!("<- {event:?}"));

        for index in 0..self.out.facts.len() {
            let Some(stamped) = self.out.facts.get(index).copied() else {
                break;
            };
            let text = stamped
                .fact
                .text()
                .map_or("", |text| self.out.text.get(text))
                .to_owned();
            self.note(&format!(
                "   == #{} {:?} {text:?}",
                stamped.seq, stamped.fact
            ));
            if stamped.seq.get() != self.last_seq + 1 {
                self.world.blame(format!(
                    "fact #{} follows fact #{}: the journal has a gap",
                    stamped.seq, self.last_seq
                ));
            }
            self.last_seq = stamped.seq.get();
            self.seen.push(Seen {
                what: What::Fact(stamped.fact),
                text,
                claimed: false,
            });
        }
        for index in 0..self.out.cmds.len() {
            let Some(command) = self.out.cmds.get(index).copied() else {
                break;
            };
            let text = command
                .text()
                .map_or("", |text| self.out.text.get(text))
                .to_owned();
            self.note(&format!("   -> {command:?} {text:?}"));
            self.seen.push(Seen {
                what: What::Cmd(command),
                text,
                claimed: false,
            });
            self.world.on_command(self.now, &command, &self.out.text);
        }
    }

    /// Delivers the playback position the kernel has not seen yet. The
    /// runtime does this ahead of every event, so that the kernel decides
    /// on what has been heard by then.
    pub(crate) fn flush_playout(&mut self) {
        for (tag, played) in self.world.reports(self.now) {
            self.step(&Event::Op(tag, played));
        }
    }

    /// Gives the kernel an event the way the runtime does.
    fn deliver(&mut self, event: &Event) {
        self.flush_playout();
        self.step(event);
        self.world.schedule_tick(self.now);
    }

    fn arrive(&mut self, arrival: Arrival) {
        match arrival {
            Arrival::Event(Event::Timer(id)) if self.world.loses(id) => {
                self.note(&format!("-- the timer {id:?} is lost"));
            }
            Arrival::Event(event) => self.deliver(&event),
            Arrival::Tick => {
                self.flush_playout();
                self.world.schedule_tick(self.now);
            }
            Arrival::Wake => {}
        }
    }

    /// Moves the clock to the next thing due at or before `deadline` and
    /// lets it happen. False if nothing is.
    fn advance(&mut self, deadline: Ms) -> bool {
        if self.world.fault().is_some() {
            return false;
        }
        let Some(pending) = self.world.pop_due(deadline) else {
            return false;
        };
        self.now = self.now.max(pending.at);
        self.arrive(pending.arrival);
        true
    }

    /// Lets `millis` pass, with everything that happens in them.
    pub(crate) fn run_for(&mut self, millis: Ms) {
        let deadline = self.now.saturating_add(millis);
        while self.advance(deadline) {}
        self.now = deadline;
    }

    /// Lets time pass until nothing more is due, for `millis` at most.
    pub(crate) fn run_to_rest(&mut self, millis: Ms) {
        let deadline = self.now.saturating_add(millis);
        while self.advance(deadline) {}
    }

    /// Lets time pass until `probe` finds what it is looking for.
    pub(crate) fn wait_for<T>(
        &mut self,
        mut probe: impl FnMut(&mut Self) -> Probe<T>,
    ) -> Result<T, String> {
        let start = self.now;
        let deadline = start.saturating_add(PATIENCE_MS);
        loop {
            if let Some(fault) = self.world.fault() {
                return Err(fault.to_owned());
            }
            match probe(self) {
                Probe::Found(found) => return Ok(found),
                Probe::Wrong(why) => return Err(why),
                Probe::NotYet => {}
            }
            if !self.advance(deadline) {
                return Err(match self.world.fault() {
                    Some(fault) => fault.to_owned(),
                    None if self.world.has_more() => format!(
                        "it had not happened {} ms later, when the scenario stopped waiting",
                        deadline - start
                    ),
                    None => format!(
                        "it never happened: nothing more was going to happen after {} ms",
                        self.now
                    ),
                });
            }
        }
    }

    /// Looks at what has happened so far, without letting time pass.
    pub(crate) fn look<T>(
        &mut self,
        probe: impl FnOnce(&mut Self) -> Probe<T>,
    ) -> Result<T, String> {
        if let Some(fault) = self.world.fault() {
            return Err(fault.to_owned());
        }
        match probe(self) {
            Probe::Found(found) => Ok(found),
            Probe::Wrong(why) => Err(why),
            Probe::NotYet => Err("it has not happened".to_owned()),
        }
    }

    /// Counts a check and turns what it found into the scenario's result.
    #[track_caller]
    pub(crate) fn settle<T>(
        &mut self,
        expected: impl Into<String>,
        outcome: Result<T, String>,
    ) -> Result<T, Box<Failure>> {
        let location = Location::caller();
        self.checks += 1;
        outcome.map_err(|found| {
            let skip = self.log.len().saturating_sub(LOG_TAIL);
            Box::new(Failure {
                ordinal: self.checks,
                expected: expected.into(),
                found,
                file: location.file(),
                line: location.line(),
                at_ms: self.now,
                path: self.chooser.borrow().taken(),
                log: self.log.iter().skip(skip).cloned().collect(),
            })
        })
    }
}
