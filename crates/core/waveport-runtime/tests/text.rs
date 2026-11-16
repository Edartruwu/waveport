//! A session of text alone, on fakes.
//!
//! Such a session has a language model and nothing to hear or speak
//! with, and here it has no transport either: what is typed comes as
//! requests, and what the agent writes goes to whoever follows the
//! session. The model, the tools and the journal are fakes on a clock the
//! test controls.
//!
//! The kernel is the one every session has. What these tests hold is
//! what a reader would notice: every word is there as it is written, an
//! answer that is typed over is on record exactly as far as it was read,
//! and the conversation is the same conversation it would have been
//! aloud.

#![expect(
    clippy::unwrap_used,
    reason = "a test that cannot be set up, or that finds something missing, fails"
)]

use core::time::Duration;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::time::Instant;
use waveport_domain::command::{Activity, ClientEvent};
use waveport_domain::config::{AgentSpec, Effect, KernelConfig, ToolSpec};
use waveport_domain::fact::Lifecycle;
use waveport_domain::ids::{GenId, ParticipantId, ToolId};
use waveport_domain::kernel::ItemView;
use waveport_domain::policy::DefaultPolicies;
use waveport_ports::journal::SessionId;
use waveport_ports::llm::LlmCaps;
use waveport_ports::plan::{PlanError, SessionPlan, Shortfall, TextPlan};
use waveport_ports::tool::ToolReply;
use waveport_runtime::context::ToolDef;
use waveport_runtime::session::{
    Brain, Ended, Parts, Request, Session, SessionEvent, SessionInput, SessionSpec, StartError,
};
use waveport_testkit::fakes::{
    FakeTransport, LlmStep, MemoryJournal, Said, ScriptedLlm, ScriptedTools,
};

const READER: ParticipantId = ParticipantId(1);
const SESSION: SessionId = SessionId(0x7E87);
const INSTRUCTIONS: &str = "You are a helpful agent in a chat.";

/// An answer long enough to be typed over: fifty words and no full stop
/// until the last, so that a policy that waits for sentences would hold
/// all of it back.
const LONG: &str = "one two three four five six seven eight nine ten eleven twelve thirteen \
                    fourteen fifteen sixteen seventeen eighteen nineteen twenty twenty-one \
                    twenty-two twenty-three twenty-four twenty-five twenty-six twenty-seven \
                    twenty-eight twenty-nine thirty thirty-one thirty-two thirty-three \
                    thirty-four thirty-five thirty-six thirty-seven thirty-eight thirty-nine \
                    forty forty-one forty-two forty-three forty-four forty-five forty-six \
                    forty-seven forty-eight forty-nine fifty.";

/// A session with one agent that has one tool.
fn spec() -> SessionSpec {
    SessionSpec {
        kernel: KernelConfig {
            agents: vec![AgentSpec {
                tools: vec![ToolId(0)],
            }],
            tools: vec![ToolSpec::new(Effect::Pure)],
            ..KernelConfig::default()
        },
        tools: vec![ToolDef {
            name: "get_time".to_owned(),
            description: "The time of day.".to_owned(),
            parameters: "{}".to_owned(),
        }],
        ..SessionSpec::new(SESSION, INSTRUCTIONS)
    }
}

/// A chat under way: the session, what it is asked through, and what a
/// follower of it is told.
struct Chat {
    session: Session,
    began: Instant,
    ask: mpsc::Sender<Request>,
    told: mpsc::Receiver<SessionEvent>,
    llm: ScriptedLlm,
    tools: ScriptedTools,
    journal: MemoryJournal,
}

impl Chat {
    /// A chat with a model that answers as `llm` says.
    async fn start(llm: ScriptedLlm) -> Self {
        Self::start_on(llm, MemoryJournal::new()).await
    }

    /// The same, on a journal that may hold an earlier life of it.
    async fn start_on(llm: ScriptedLlm, journal: MemoryJournal) -> Self {
        let began = Instant::now();
        let tools = ScriptedTools::with(|_| ToolReply::Completed("09:15".to_owned()));
        let parts = Parts {
            brain: Brain::Text {
                llm: Arc::new(llm.clone()),
            },
            transport: None,
            tools: Arc::new(tools.clone()),
            journal: Arc::new(journal.clone()),
            sink: None,
            taps: Vec::new(),
        };
        let mut session = Session::open(spec(), parts, DefaultPolicies::default())
            .await
            .unwrap();
        let (ask, inbox) = mpsc::channel(16);
        session.listen_to(inbox);
        let (to, told) = mpsc::channel(1024);
        session.follow(session.view().next_seq(), to);
        Self {
            session,
            began,
            ask,
            told,
            llm,
            tools,
            journal,
        }
    }

    /// The reader types `text`.
    fn types(&self, text: &str) {
        let typed = SessionInput::Text {
            who: READER,
            text: text.to_owned(),
        };
        self.ask.try_send(typed.into()).unwrap();
    }

    /// Runs the session until `ms` after its start, or until it ends.
    async fn until(&mut self, ms: u64) -> Option<Ended> {
        let at = self.began + Duration::from_millis(ms);
        tokio::time::timeout_at(at, self.session.run()).await.ok()
    }

    /// What the follower was told since it was last asked.
    fn told(&mut self) -> Vec<SessionEvent> {
        let mut told = Vec::new();
        while let Ok(event) = self.told.try_recv() {
            told.push(event);
        }
        told
    }

    /// The conversation as the kernel has it.
    fn conversation(&self) -> Vec<Said> {
        self.session
            .view()
            .items()
            .map(|item| match item {
                ItemView::UserTurn { who, text, .. } => Said::User {
                    speaker: Some(who),
                    text: text.to_owned(),
                },
                ItemView::Utterance { text, .. } => Said::Assistant {
                    text: text.to_owned(),
                },
                ItemView::ToolCall {
                    call, tool, args, ..
                } => Said::ToolCall {
                    call,
                    tool: format!("tool {}", tool.get()),
                    args: args.to_owned(),
                },
                ItemView::ToolResult { call, text, .. } => Said::ToolResult {
                    call,
                    text: text.to_owned(),
                },
                ItemView::Notice { text, .. } => Said::Notice {
                    text: text.to_owned(),
                },
            })
            .collect()
    }

    /// What the agent wrote, as far as it was read, and whether it was
    /// cut off.
    fn utterances(&self) -> Vec<(String, bool)> {
        self.session
            .view()
            .items()
            .filter_map(|item| match item {
                ItemView::Utterance {
                    text, truncated, ..
                } => Some((text.to_owned(), truncated)),
                _ => None,
            })
            .collect()
    }
}

fn user(text: &str) -> Said {
    Said::User {
        speaker: Some(READER),
        text: text.to_owned(),
    }
}

fn assistant(text: &str) -> Said {
    Said::Assistant {
        text: text.to_owned(),
    }
}

/// The text of each response that a follower was told, in the order the
/// responses were first told of.
fn written(told: &[SessionEvent]) -> Vec<(GenId, String)> {
    let mut written: Vec<(GenId, String)> = Vec::new();
    for event in told {
        let SessionEvent::Said { generation, text } = event else {
            continue;
        };
        match written.iter_mut().find(|(of, _)| of == generation) {
            Some((_, so_far)) => so_far.push_str(text),
            None => written.push((*generation, text.clone())),
        }
    }
    written
}

/// How many pieces of text a follower was told.
fn pieces(told: &[SessionEvent]) -> usize {
    told.iter()
        .filter(|event| matches!(event, SessionEvent::Said { .. }))
        .count()
}

#[tokio::test(start_paused = true)]
async fn what_is_typed_is_answered_in_text() {
    let llm = ScriptedLlm::replies(vec![
        vec![LlmStep::say("Hello! How can I help you today?")],
        vec![LlmStep::say("It is a quarter past nine.")],
    ]);
    let mut chat = Chat::start(llm).await;
    // Such a session is planned without a transport, and nothing about
    // it falls short.
    assert_eq!(chat.session.plan(), &SessionPlan::Text(TextPlan::default()));

    chat.types("hello");
    assert_eq!(chat.until(1_000).await, None);
    chat.types("what time is it");
    assert_eq!(chat.until(2_000).await, None);

    // The conversation is what it would have been aloud, and whoever
    // wrote was taken to be there.
    assert_eq!(
        chat.conversation(),
        [
            user("hello"),
            assistant("Hello! How can I help you today?"),
            user("what time is it"),
            assistant("It is a quarter past nine."),
        ]
    );
    assert_eq!(
        chat.utterances()
            .iter()
            .map(|(_, cut)| *cut)
            .collect::<Vec<_>>(),
        [false, false]
    );
    assert!(chat.session.view().participants().any(|who| who == READER));
    let asked = chat.llm.asked();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[0].last_user(), Some("hello"));
    assert_eq!(asked[1].last_user(), Some("what time is it"));
    assert_eq!(
        asked[1].messages.len(),
        3,
        "it is asked with what went before"
    );

    // The follower was told each response, whole, and what the agent was
    // doing.
    let told = chat.told();
    let responses: Vec<String> = written(&told).into_iter().map(|(_, text)| text).collect();
    assert_eq!(
        responses,
        [
            "Hello! How can I help you today?",
            "It is a quarter past nine."
        ]
    );
    let doing: Vec<Activity> = told
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Told(ClientEvent::Agent { activity, .. }) => Some(*activity),
            _ => None,
        })
        .collect();
    assert_eq!(
        doing,
        [
            Activity::Thinking,
            Activity::Speaking,
            Activity::Listening,
            Activity::Thinking,
            Activity::Speaking,
            Activity::Listening,
        ]
    );
    // Nothing of a session of text is counted as gone wrong.
    assert_eq!(chat.session.stats().strays, 0);
}

#[tokio::test(start_paused = true)]
async fn every_word_is_there_as_it_is_written() {
    // The model writes a word every 20 ms and no sentence ends before
    // the last word. The policy the session was given waits for whole
    // sentences, which is right for a synthesiser and wrong for a reader.
    let llm = ScriptedLlm::replies(vec![vec![LlmStep::say(LONG)]]);
    let mut chat = Chat::start(llm).await;
    chat.types("count to fifty");

    // 200 ms to the first word, and then one every 20 ms.
    assert_eq!(chat.until(190).await, None);
    assert_eq!(pieces(&chat.told()), 0);
    assert_eq!(chat.until(510).await, None);
    let told = chat.told();
    let so_far = written(&told);
    assert_eq!(so_far.len(), 1);
    let text = &so_far[0].1;
    assert!(LONG.starts_with(text.as_str()), "{text:?}");
    let words = text.split_whitespace().count();
    assert!(
        (14..=17).contains(&words),
        "{words} words by 510 ms: {text:?}"
    );
    assert_eq!(pieces(&told), words, "a word at a time");

    assert_eq!(chat.until(3_000).await, None);
    let rest = written(&chat.told());
    assert_eq!(format!("{text}{}", rest[0].1), LONG);
    assert_eq!(chat.utterances(), [(LONG.to_owned(), false)]);
}

#[tokio::test(start_paused = true)]
async fn an_answer_that_is_typed_over_is_on_record_as_far_as_it_was_read() {
    let llm = ScriptedLlm::replies(vec![
        vec![LlmStep::say(LONG)],
        vec![LlmStep::say("All right, I have stopped.")],
    ]);
    let mut chat = Chat::start(llm).await;
    chat.types("count to fifty");
    assert_eq!(chat.until(600).await, None);
    // The reader has about twenty words when they type again.
    chat.types("stop");
    assert_eq!(chat.until(3_000).await, None);

    let told = chat.told();
    let responses = written(&told);
    assert_eq!(responses.len(), 2);
    let (first, given) = &responses[0];
    let words = given.split_whitespace().count();
    assert!((19..=22).contains(&words), "{words} words: {given:?}");
    assert_eq!(responses[1].1, "All right, I have stopped.");

    // What is on record of the first answer is what the reader was
    // given, to the letter but for the space at its end, and it says
    // that it was cut off.
    let read = given.trim_end().to_owned();
    assert!(given.len() - read.len() <= 1, "{given:?}");
    assert_eq!(
        chat.utterances(),
        [
            (read.clone(), true),
            ("All right, I have stopped.".to_owned(), false)
        ]
    );
    assert_eq!(
        chat.conversation(),
        [
            user("count to fifty"),
            assistant(&read),
            user("stop"),
            assistant("All right, I have stopped."),
        ]
    );
    // And nothing more of it was told once it had been stopped: every
    // piece of the first answer comes before the first of the second.
    let last_of_first = told
        .iter()
        .rposition(
            |event| matches!(event, SessionEvent::Said { generation, .. } if generation == first),
        )
        .unwrap();
    let first_of_second = told
        .iter()
        .position(
            |event| matches!(event, SessionEvent::Said { generation, .. } if generation != first),
        )
        .unwrap();
    assert!(last_of_first < first_of_second);
    // The model was told what the reader had read, and no more.
    let asked = chat.llm.asked();
    assert_eq!(asked.len(), 2);
    assert!(
        asked[1]
            .messages
            .contains(&Said::Assistant { text: read.clone() }),
        "{:?}",
        asked[1].messages
    );
}

#[tokio::test(start_paused = true)]
async fn a_second_message_before_the_first_is_answered_gets_one_answer() {
    let llm = ScriptedLlm::with(|asked| {
        let all: Vec<&str> = asked
            .messages
            .iter()
            .filter_map(|said| match said {
                Said::User { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        vec![LlmStep::say(&format!("You wrote: {}.", all.join(" and ")))]
    });
    let mut chat = Chat::start(llm).await;
    chat.types("my order is late");
    // The model has not written a word yet.
    assert_eq!(chat.until(100).await, None);
    chat.types("it is order 1234");
    assert_eq!(chat.until(2_000).await, None);

    assert_eq!(
        chat.conversation(),
        [
            user("my order is late"),
            user("it is order 1234"),
            assistant("You wrote: my order is late and it is order 1234."),
        ]
    );
    // The first request was let go of, and nothing of an answer to it
    // was told.
    assert_eq!(chat.llm.asked().len(), 2);
    assert_eq!(written(&chat.told()).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_tool_is_called_and_its_answer_written() {
    let llm = ScriptedLlm::replies(vec![
        vec![
            LlmStep::say("Let me look."),
            LlmStep::call("get_time", "{}"),
        ],
        vec![LlmStep::say("It is 09:15.")],
    ]);
    let mut chat = Chat::start(llm).await;
    chat.types("what time is it");
    assert_eq!(chat.until(3_000).await, None);

    assert_eq!(chat.tools.called().len(), 1);
    // As aloud, a call enters the conversation when it is made, and the
    // words of its response when the response is over.
    let conversation = chat.conversation();
    assert_eq!(conversation.len(), 5, "{conversation:?}");
    assert_eq!(conversation[0], user("what time is it"));
    assert!(matches!(conversation[1], Said::ToolCall { .. }));
    assert_eq!(conversation[2], assistant("Let me look."));
    assert!(matches!(&conversation[3], Said::ToolResult { text, .. } if text == "09:15"));
    assert_eq!(conversation[4], assistant("It is 09:15."));
    let responses: Vec<String> = written(&chat.told())
        .into_iter()
        .map(|(_, text)| text)
        .collect();
    assert_eq!(responses, ["Let me look.", "It is 09:15."]);
}

#[tokio::test(start_paused = true)]
async fn the_agent_writes_what_it_is_told_to_say() {
    let mut chat = Chat::start(ScriptedLlm::replies(vec![])).await;
    // Nobody has written yet, so nobody has been taken to be there, and
    // the agent can still be made to speak first.
    let greeting = "Welcome. What can I do for you?".to_owned();
    chat.ask
        .try_send(SessionInput::Say { text: greeting }.into())
        .unwrap();
    assert_eq!(chat.until(500).await, None);
    assert_eq!(
        chat.conversation(),
        [assistant("Welcome. What can I do for you?")]
    );
    let responses: Vec<String> = written(&chat.told())
        .into_iter()
        .map(|(_, text)| text)
        .collect();
    assert_eq!(responses, ["Welcome. What can I do for you?"]);
    assert_eq!(chat.llm.asked().len(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_chat_goes_on_from_its_journal() {
    let journal = MemoryJournal::new();
    let llm = ScriptedLlm::replies(vec![vec![LlmStep::say("Hello! How can I help?")]]);
    let mut chat = Chat::start_on(llm, journal.clone()).await;
    chat.types("hello");
    assert_eq!(chat.until(1_000).await, None);
    let before = chat.conversation();
    assert_eq!(before.len(), 2);
    // The process is gone, and with it everything but the journal.
    drop(chat);

    let llm = ScriptedLlm::replies(vec![vec![LlmStep::say("You said hello.")]]);
    let mut chat = Chat::start_on(llm, journal).await;
    assert!(chat.session.rebuilt().is_some());
    assert_eq!(chat.conversation(), before);
    // The reader is still there, and is answered with what went before.
    chat.types("what did I say");
    assert_eq!(chat.until(2_000).await, None);
    assert_eq!(
        chat.conversation(),
        [
            user("hello"),
            assistant("Hello! How can I help?"),
            user("what did I say"),
            assistant("You said hello."),
        ]
    );
    let asked = chat.llm.asked();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].messages.len(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_chat_closes_when_it_is_asked_to() {
    let llm = ScriptedLlm::replies(vec![vec![LlmStep::say(LONG)]]);
    let mut chat = Chat::start(llm).await;
    chat.types("count to fifty");
    assert_eq!(chat.until(400).await, None);
    // It is asked to close in the middle of an answer, which it finishes
    // first.
    chat.ask.try_send(SessionInput::Close.into()).unwrap();
    assert_eq!(chat.until(30_000).await, Some(Ended::Closed));
    assert!(chat.began.elapsed() < Duration::from_secs(3));
    assert_eq!(chat.utterances(), [(LONG.to_owned(), false)]);
    assert_eq!(chat.session.view().lifecycle(), Lifecycle::Closed);
    let told = chat.told();
    assert!(told.contains(&SessionEvent::Told(ClientEvent::Lifecycle(
        Lifecycle::Closed
    ))));
    // Everything it decided is in the journal.
    assert!(chat.journal.records(SESSION).len() > 4);
}

#[tokio::test(start_paused = true)]
async fn a_chat_is_planned_for_the_parts_it_has() {
    let open = async |llm: ScriptedLlm, transport: Option<FakeTransport>| {
        let parts = Parts {
            brain: Brain::Text { llm: Arc::new(llm) },
            transport: transport.map(
                |transport| -> Box<dyn waveport_ports::transport::Transport> {
                    Box::new(transport)
                },
            ),
            tools: Arc::new(ScriptedTools::none()),
            journal: Arc::new(MemoryJournal::new()),
            sink: None,
            taps: Vec::new(),
        };
        Session::open(spec(), parts, DefaultPolicies::default()).await
    };

    // An agent with tools needs a model that can call them, as in every
    // shape.
    let mut cannot_call = ScriptedLlm::replies(vec![]);
    cannot_call.caps = LlmCaps {
        tools: false,
        parallel_tools: false,
    };
    let refused = open(cannot_call, None).await.map(|_| ()).unwrap_err();
    assert_eq!(refused, StartError::Plan(PlanError::NoTools));

    // On a transport that carries audio nobody is heard. The session is
    // started, and its plan says so.
    let (line, _far_end) = FakeTransport::sound_card().join(0, READER).build();
    let session = open(ScriptedLlm::replies(vec![]), Some(line))
        .await
        .unwrap();
    assert_eq!(session.plan().shortfalls(), [Shortfall::AudioUnheard]);
}
