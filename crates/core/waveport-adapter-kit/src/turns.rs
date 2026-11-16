//! What the marks of a session add up to.
//!
//! A session says when each part of a turn was reached, and what it
//! recorded. Whoever watches a session wants to know how long the parts
//! took: how long the caller waited for an answer, and where the time
//! went. A [`Tracker`] is told what one session's sink is told and gives
//! back each [`Stretch`] as it ends.
//!
//! It keeps a small, fixed amount about a session, so it can be called
//! on the session's own thread: nothing here waits, and nothing is
//! allocated once a tracker exists.

use core::time::Duration;

use waveport_domain::fact::{Fact, Lifecycle, Stamped};
use waveport_domain::ids::{GenId, ToolCallId, TurnId};
use waveport_domain::time::MonoTime;
use waveport_ports::telemetry::{Stage, StageMark};

/// How many responses are followed at once. A session has at most four
/// under way.
const RESPONSES: usize = 8;

/// How many tool calls are followed at once.
const CALLS: usize = 32;

/// A part of a session that began and ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Stretch {
    /// Which part.
    pub what: What,
    /// When it began.
    pub from: MonoTime,
    /// When it ended.
    pub to: MonoTime,
}

impl Stretch {
    /// How long it took.
    #[must_use]
    pub const fn took(&self) -> Duration {
        self.to.since(self.from)
    }
}

/// The parts of a session that are timed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum What {
    /// From the last final transcript of a turn to the decision that the
    /// turn has ended.
    Deciding(TurnId),
    /// From the end of a turn to the first frame of the answer handed to
    /// the transport: what the caller waited.
    Answering(TurnId, GenId),
    /// From asking the model for a response to the first it wrote.
    Thinking(GenId),
    /// From the first the model wrote of a response to the first text
    /// given to speech synthesis.
    Phrasing(GenId),
    /// From the first text given to speech synthesis to the first audio
    /// it delivered.
    Voicing(GenId),
    /// From the first audio of a response to its first frame handed to
    /// the transport.
    Sending(GenId),
    /// From lowering a response, because someone spoke over it, to
    /// stopping it.
    Yielding(GenId),
    /// From the model asking for a tool call to the call being sent: the
    /// wait for the call to be allowed and for its record to be stored.
    Clearing(ToolCallId),
    /// From sending a tool call to its coming back.
    Calling(ToolCallId),
}

impl What {
    /// A name for the kind of part, the same for every turn: for a
    /// column, a metric or a key.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Deciding(_) => "deciding",
            Self::Answering(..) => "answering",
            Self::Thinking(_) => "thinking",
            Self::Phrasing(_) => "phrasing",
            Self::Voicing(_) => "voicing",
            Self::Sending(_) => "sending",
            Self::Yielding(_) => "yielding",
            Self::Clearing(_) => "clearing",
            Self::Calling(_) => "calling",
        }
    }
}

/// What is known of a response that is under way.
#[derive(Clone, Copy, Debug)]
struct Response {
    generation: GenId,
    asked: Option<MonoTime>,
    token: Option<MonoTime>,
    text: Option<MonoTime>,
    audio: Option<MonoTime>,
    lowered: Option<MonoTime>,
}

/// What is known of a tool call that is under way.
#[derive(Clone, Copy, Debug)]
struct Call {
    id: ToolCallId,
    proposed: Option<MonoTime>,
    sent: Option<MonoTime>,
}

/// Follows one session.
#[derive(Debug)]
pub struct Tracker {
    /// The turn whose final transcript came last, and when.
    heard: Option<(TurnId, MonoTime)>,
    /// The turn that has ended and has not been answered aloud, and when
    /// it ended.
    owed: Option<(TurnId, MonoTime)>,
    responses: Vec<Response>,
    calls: Vec<Call>,
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Tracker {
    /// A tracker that has seen nothing.
    #[must_use]
    pub fn new() -> Self {
        Self {
            heard: None,
            owed: None,
            responses: Vec::with_capacity(RESPONSES),
            calls: Vec::with_capacity(CALLS),
        }
    }

    /// The turn that has ended and has not been answered aloud.
    #[must_use]
    pub fn owed(&self) -> Option<TurnId> {
        self.owed.map(|(turn, _)| turn)
    }

    /// Takes a mark. Each part that ends with it is given to `found`.
    pub fn mark(&mut self, mark: StageMark, mut found: impl FnMut(Stretch)) {
        let to = mark.at;
        let mut stretch = |what, from: Option<MonoTime>| {
            if let Some(from) = from {
                found(Stretch { what, from, to });
            }
        };
        match mark.stage {
            Stage::TranscriptFinal(turn) => self.heard = Some((turn, to)),
            Stage::TurnEnded(turn) => {
                let heard = self.heard.take().filter(|(of, _)| *of == turn);
                stretch(What::Deciding(turn), heard.map(|(_, at)| at));
                self.owed = Some((turn, to));
            }
            Stage::Asked(generation) => {
                let mut response = self.response(generation);
                response.asked = Some(to);
                self.keep(response);
            }
            Stage::FirstToken(generation) => {
                let mut response = self.response(generation);
                response.token = Some(to);
                self.keep(response);
                stretch(What::Thinking(generation), response.asked);
            }
            Stage::FirstText(generation) => {
                let mut response = self.response(generation);
                response.text = Some(to);
                self.keep(response);
                stretch(What::Phrasing(generation), response.token);
            }
            Stage::FirstAudio(generation) => {
                let mut response = self.response(generation);
                response.audio = Some(to);
                self.keep(response);
                stretch(What::Voicing(generation), response.text);
            }
            Stage::FirstFrameOut(generation) => {
                stretch(What::Sending(generation), self.response(generation).audio);
                if let Some((turn, ended)) = self.owed.take() {
                    stretch(What::Answering(turn, generation), Some(ended));
                }
            }
            Stage::Lowered(generation) => {
                let mut response = self.response(generation);
                response.lowered = response.lowered.or(Some(to));
                self.keep(response);
            }
            Stage::Stopped(generation) => {
                let mut response = self.response(generation);
                stretch(What::Yielding(generation), response.lowered.take());
                self.keep(response);
            }
            Stage::ToolSent(id) => {
                let mut call = self.call(id);
                stretch(What::Clearing(id), call.proposed.take());
                call.sent = Some(to);
                self.keep_call(call);
            }
            Stage::ToolBack(id) => {
                let mut call = self.call(id);
                stretch(What::Calling(id), call.sent.take());
                self.keep_call(call);
            }
            Stage::Stored(_) => {}
        }
    }

    /// Takes the facts of one step, recorded at `at`.
    pub fn facts(&mut self, at: MonoTime, facts: &[Stamped]) {
        for stamped in facts {
            match stamped.fact {
                // A turn that was said to someone else is not answered.
                Fact::TurnCommitted {
                    turn,
                    addressed: false,
                    ..
                } => {
                    if self.owed.is_some_and(|(owed, _)| owed == turn) {
                        self.owed = None;
                    }
                }
                Fact::ToolProposed { call: id, .. } => {
                    let mut call = self.call(id);
                    call.proposed = Some(at);
                    self.keep_call(call);
                }
                Fact::ToolFinished { call: id, .. } => self.calls.retain(|call| call.id != id),
                Fact::UtteranceCommitted { generation, .. } => {
                    self.responses
                        .retain(|response| response.generation != generation);
                }
                Fact::LifecycleChanged {
                    to: Lifecycle::Closed,
                    ..
                } => {
                    self.heard = None;
                    self.owed = None;
                    self.responses.clear();
                    self.calls.clear();
                }
                _ => {}
            }
        }
    }

    /// What is known of the response `generation`.
    fn response(&self, generation: GenId) -> Response {
        let known = self.responses.iter().copied();
        let unknown = Response {
            generation,
            asked: None,
            token: None,
            text: None,
            audio: None,
            lowered: None,
        };
        known
            .rev()
            .find(|response| response.generation == generation)
            .unwrap_or(unknown)
    }

    /// Keeps what is known of a response. The oldest makes room for one
    /// that is new.
    fn keep(&mut self, response: Response) {
        let known = self.responses.iter_mut();
        if let Some(known) = known
            .rev()
            .find(|known| known.generation == response.generation)
        {
            *known = response;
            return;
        }
        if self.responses.len() >= RESPONSES {
            self.responses.remove(0);
        }
        self.responses.push(response);
    }

    /// What is known of the tool call `id`.
    fn call(&self, id: ToolCallId) -> Call {
        let unknown = Call {
            id,
            proposed: None,
            sent: None,
        };
        let known = self.calls.iter().copied();
        known.rev().find(|call| call.id == id).unwrap_or(unknown)
    }

    /// Keeps what is known of a tool call. The oldest makes room for one
    /// that is new.
    fn keep_call(&mut self, call: Call) {
        if let Some(known) = self
            .calls
            .iter_mut()
            .rev()
            .find(|known| known.id == call.id)
        {
            *known = call;
            return;
        }
        if self.calls.len() >= CALLS {
            self.calls.remove(0);
        }
        self.calls.push(call);
    }
}

#[cfg(test)]
mod tests {
    use waveport_domain::event::CloseReason;
    use waveport_domain::fact::{Fact, Lifecycle, Stamped, ToolOutcome};
    use waveport_domain::ids::{
        AgentId, BatchId, GenId, ParticipantId, Seq, ToolCallId, ToolId, TurnId,
    };
    use waveport_domain::text::TextArena;
    use waveport_domain::time::{MediaSpan, MediaTime, MonoTime};
    use waveport_ports::telemetry::{Stage, StageMark};

    use super::{Stretch, Tracker, What};

    /// What a tracker found in the marks it was given, as the name of
    /// each part and how long it took in milliseconds.
    #[derive(Default)]
    struct Told {
        tracker: Tracker,
        found: Vec<Stretch>,
        text: TextArena,
    }

    impl Told {
        fn mark(&mut self, at_ms: u64, stage: Stage) {
            let at = MonoTime::from_millis(at_ms);
            let found = &mut self.found;
            self.tracker
                .mark(StageMark { at, stage }, |stretch| found.push(stretch));
        }

        fn fact(&mut self, at_ms: u64, fact: impl FnOnce(&mut TextArena) -> Fact) {
            let fact = fact(&mut self.text);
            let stamped = Stamped { seq: Seq(1), fact };
            self.tracker.facts(MonoTime::from_millis(at_ms), &[stamped]);
        }

        fn took(&self) -> Vec<(&'static str, u128)> {
            self.found
                .iter()
                .map(|stretch| (stretch.what.name(), stretch.took().as_millis()))
                .collect()
        }
    }

    fn turn_committed(turn: u32, addressed: bool) -> impl FnOnce(&mut TextArena) -> Fact {
        move |text| Fact::TurnCommitted {
            turn: TurnId(turn),
            who: ParticipantId(1),
            span: MediaSpan::new(MediaTime::from_millis(0), MediaTime::from_millis(900)),
            text: text.push("What time is it?"),
            addressed,
        }
    }

    fn utterance(generation: u32) -> impl FnOnce(&mut TextArena) -> Fact {
        move |text| Fact::UtteranceCommitted {
            generation: GenId(generation),
            agent: AgentId(0),
            answers: None,
            text: text.push("It is nine."),
            truncated: false,
        }
    }

    #[test]
    fn a_turn_with_a_tool_call_is_taken_apart() {
        let mut told = Told::default();
        let (turn, first, second, call) = (TurnId(1), GenId(1), GenId(2), ToolCallId(7));
        told.mark(2_400, Stage::TranscriptFinal(turn));
        told.mark(2_470, Stage::TurnEnded(turn));
        told.fact(2_470, turn_committed(1, true));
        assert_eq!(told.tracker.owed(), Some(turn));
        told.mark(2_470, Stage::Asked(first));
        told.mark(2_670, Stage::FirstToken(first));
        told.fact(2_670, |text| Fact::ToolProposed {
            call,
            batch: BatchId(1),
            generation: first,
            tool: ToolId(0),
            args: text.push("{}"),
        });
        told.mark(2_675, Stage::Stored(Seq(4)));
        told.mark(2_675, Stage::ToolSent(call));
        told.mark(2_825, Stage::ToolBack(call));
        told.fact(2_825, |text| Fact::ToolFinished {
            call,
            outcome: ToolOutcome::Completed,
            result: text.push("09:00"),
        });
        told.mark(2_825, Stage::Asked(second));
        told.mark(3_025, Stage::FirstToken(second));
        told.mark(3_045, Stage::FirstText(second));
        told.mark(3_145, Stage::FirstAudio(second));
        told.mark(3_150, Stage::FirstFrameOut(second));
        assert_eq!(told.tracker.owed(), None);
        told.fact(4_000, utterance(2));

        assert_eq!(
            told.took(),
            [
                ("deciding", 70),
                ("thinking", 200),
                ("clearing", 5),
                ("calling", 150),
                ("thinking", 200),
                ("phrasing", 20),
                ("voicing", 100),
                ("sending", 5),
                ("answering", 680),
            ]
        );
        let last = told.found.last().copied();
        assert_eq!(last.map(|it| it.what), Some(What::Answering(turn, second)));
        assert_eq!(last.map(|it| it.from), Some(MonoTime::from_millis(2_470)));
        assert_eq!(last.map(|it| it.to), Some(MonoTime::from_millis(3_150)));
    }

    #[test]
    fn a_response_that_is_spoken_over_is_timed_from_lowering_to_stopping() {
        let mut told = Told::default();
        let story = GenId(3);
        told.mark(1_000, Stage::FirstText(story));
        told.mark(1_100, Stage::FirstAudio(story));
        told.mark(1_100, Stage::FirstFrameOut(story));
        told.mark(5_000, Stage::Lowered(story));
        // Lowered once: a second mark does not move the first.
        told.mark(5_100, Stage::Lowered(story));
        told.mark(5_240, Stage::Stopped(story));
        // What was said with no model and to no turn has no thinking,
        // no phrasing and no answering.
        assert_eq!(
            told.took(),
            [("voicing", 100), ("sending", 0), ("yielding", 240)]
        );
        // A response that is stopped without having been lowered was
        // not yielded.
        told.mark(6_000, Stage::Stopped(story));
        assert_eq!(told.found.len(), 3);
    }

    #[test]
    fn only_the_first_sound_after_a_turn_answers_it() {
        let mut told = Told::default();
        told.mark(1_000, Stage::TurnEnded(TurnId(1)));
        told.mark(1_500, Stage::FirstFrameOut(GenId(1)));
        told.mark(9_000, Stage::FirstFrameOut(GenId(2)));
        assert_eq!(told.took(), [("answering", 500)]);

        // A turn that was said to someone else is owed no answer, so
        // what the agent says next is not one.
        told.mark(10_000, Stage::TurnEnded(TurnId(2)));
        told.fact(10_000, turn_committed(2, false));
        assert_eq!(told.tracker.owed(), None);
        told.mark(12_000, Stage::FirstFrameOut(GenId(3)));
        assert_eq!(told.found.len(), 1);

        // The final transcript of another turn says nothing of this one.
        told.mark(13_000, Stage::TranscriptFinal(TurnId(3)));
        told.mark(13_500, Stage::TurnEnded(TurnId(4)));
        assert_eq!(told.found.len(), 1);
    }

    #[test]
    fn what_is_followed_is_bounded_and_forgotten_when_it_is_over() {
        let mut told = Told::default();
        // More responses and calls than are followed: the oldest make
        // room, and nothing grows.
        for n in 0..100 {
            told.mark(n, Stage::Asked(GenId(u32::try_from(n).unwrap_or(0))));
            told.mark(n, Stage::ToolSent(ToolCallId(n)));
        }
        assert_eq!(told.tracker.responses.len(), super::RESPONSES);
        assert_eq!(told.tracker.calls.len(), super::CALLS);
        assert_eq!(told.tracker.responses.capacity(), super::RESPONSES);
        assert_eq!(told.tracker.calls.capacity(), super::CALLS);

        // The newest are still followed.
        told.mark(300, Stage::FirstToken(GenId(99)));
        told.mark(300, Stage::ToolBack(ToolCallId(99)));
        assert_eq!(told.took(), [("thinking", 201), ("calling", 201)]);

        // A response whose utterance is recorded is over.
        told.fact(400, utterance(99));
        assert_eq!(told.tracker.responses.len(), super::RESPONSES - 1);

        // And a session that is closed is followed no more.
        told.mark(500, Stage::TurnEnded(TurnId(9)));
        told.fact(600, |_| Fact::LifecycleChanged {
            to: Lifecycle::Closed,
            reason: Some(CloseReason::Requested),
        });
        assert!(told.tracker.responses.is_empty() && told.tracker.calls.is_empty());
        assert_eq!(told.tracker.owed(), None);
    }
}
