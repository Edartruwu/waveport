//! Calls taken over a WebSocket: the agent is here, and each caller is a
//! `waveport client` somewhere else.
//!
//! A caller speaks client protocol v1. Its connection may drop: the call
//! waits for it, and it comes back with the token it was given as the
//! call began. A caller whose call is under way here is given its line
//! back. One whose call broke off with the command that ran it is given
//! the call back from what was kept of it, if the command was told to go
//! on with such calls.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::mpsc;
use tokio::task::JoinSet;
use waveport_assembly::ResumeToken;
use waveport_assembly::shown::{self, Asked, Follower, Position};
use waveport_domain::ids::ParticipantId;
use waveport_ports::control::SessionInput;
use waveport_ports::journal::SessionId;
use waveport_protocol::client::v1::{ErrorCode, FromClient, Resume};
use waveport_transport_websocket::{
    Caller, GREETING_TIMEOUT, Handle, Knock, Link, Listener, Welcome,
};

use crate::args::Serve;
use crate::call::{self, Outcome, Setup};
use crate::{Context, Failure};

/// Who is at the far end of a call.
const CALLER: ParticipantId = ParticipantId(0);

/// A call that is under way.
struct Live {
    /// Gives the call its caller back.
    handle: Handle,
    /// The number the call is told under on this terminal.
    number: u32,
    /// Which life of the call this is.
    life: u64,
    /// How long it waits for a caller whose line dropped.
    grace_ms: u32,
}

/// What the calls of one server share.
struct Calls {
    stack: Setup,
    /// How many calls may be taken in all, if that is limited.
    at_most: Option<u32>,
    /// How many have been.
    taken: AtomicU32,
    /// The calls that are under way.
    live: Mutex<BTreeMap<SessionId, Live>>,
    /// Whether a call that broke off with the command that ran it is
    /// gone on with when its caller comes back.
    resume: bool,
    /// Whether a caller is shown what the tools were given and what
    /// they answered: one that could be anybody is not.
    tool_text: bool,
}

impl Calls {
    fn live(&self) -> std::sync::MutexGuard<'_, BTreeMap<SessionId, Live>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether as many calls have been taken as may be.
    fn full(&self) -> bool {
        self.at_most
            .is_some_and(|most| self.taken.load(Ordering::Relaxed) >= most)
    }

    /// Counts one more call as taken. Returns its number, or `None` if
    /// no more are taken: two that say hello at once may both have
    /// found room, and only as many as may be are taken.
    fn take(&self) -> Option<u32> {
        let number = self.taken.fetch_add(1, Ordering::Relaxed) + 1;
        self.at_most
            .is_none_or(|most| number <= most)
            .then_some(number)
    }

    /// What a caller of the call `id` is welcomed with. `back` is the
    /// token of one that came back with it, which it keeps: a welcome
    /// that never arrived then costs it nothing. A caller that is new
    /// to the call is given one.
    async fn welcome(
        &self,
        id: SessionId,
        live: &Live,
        asked: Asked,
        back: Option<&Resume>,
    ) -> Welcome {
        let stack = self.stack.stack();
        let resume_token = match back {
            Some(resume) => Some(resume.token.clone()),
            None => match stack.kept.credential(id).await {
                Ok(token) => Some(token.to_string()),
                Err(Failure(failure)) => {
                    let number = live.number;
                    eprintln!("waveport: [{number}] the caller cannot come back: {failure}");
                    None
                }
            },
        };
        Welcome {
            session: id.to_string(),
            life: live.life,
            resumed: back.is_some(),
            resume_token,
            features: asked.features(),
            next_seq: stack.status(id).map_or(1, |status| status.next_seq.get()),
            grace_ms: Some(live.grace_ms),
        }
    }
}

/// Takes calls until it is told to stop, or has taken as many as it was
/// told to.
pub(crate) async fn serve(args: Serve) -> Result<Outcome, Failure> {
    let stack = Setup::build(&args.agent, Some(&args.aloud), args.resume).await?;
    for note in stack.notes() {
        println!("note: {note}");
    }
    let listener = Listener::bind(args.listen, args.exposed)
        .await
        .map_err(|error| Failure(format!("{error} (--exposed allows it)")))?;
    let address = listener
        .address()
        .with(|| "the system does not say where calls are taken".to_owned())?;
    // Whoever started the command reads this line to learn the port.
    println!("taking calls at ws://{address}");
    let calls = Arc::new(Calls {
        stack,
        at_most: args.calls,
        taken: AtomicU32::new(0),
        live: Mutex::default(),
        resume: args.resume,
        tool_text: !args.exposed,
    });

    let mut running = JoinSet::new();
    let mut ending = false;
    let mut went = Outcome::Ended;
    loop {
        if (ending || calls.full()) && running.is_empty() {
            break;
        }
        tokio::select! {
            // A caller that comes back is taken for as long as its call
            // runs, whether or not another call would be.
            knock = listener.accept(), if !ending => match knock {
                Ok(knock) => {
                    running.spawn(one(Arc::clone(&calls), knock));
                }
                Err(error) => eprintln!("waveport: a connection could not be taken: {error}"),
            },
            Some(done) = running.join_next() => {
                if !matches!(done, Ok(None | Some(Outcome::Ended))) {
                    went = Outcome::Broken;
                }
            }
            _ = tokio::signal::ctrl_c(), if !ending => {
                ending = true;
                eprintln!("waveport: ending the calls. Ctrl-C again stops them where they are.");
                for id in calls.live().keys() {
                    calls.stack.ask_to_end(*id);
                }
            }
            () = call::last_words(ending) => {
                eprintln!("waveport: the calls were stopped where they were");
                calls.stack.finish().await;
                return Ok(Outcome::Broken);
            }
        }
    }
    calls.stack.finish().await;
    Ok(went)
}

/// One connection: a call, if whoever made it says hello and there is
/// room for one, or the line of a call that is under way. Returns how
/// the call went, or nothing if the connection began none.
async fn one(calls: Arc<Calls>, knock: Knock) -> Option<Outcome> {
    let from = knock.from();
    let caller = match knock.greet(GREETING_TIMEOUT).await {
        Ok(caller) => caller,
        Err(error) => {
            eprintln!("waveport: a connection from {from} was not a call: {error}");
            return None;
        }
    };
    if let Some(resume) = caller.hello().resume.clone() {
        return comes_back(calls, caller, &resume, from).await;
    }
    let Some(number) = calls.take() else {
        caller
            .refuse(ErrorCode::Busy, "no more calls are taken")
            .await;
        return None;
    };
    run(calls, caller, call::name(), number, None, from).await
}

/// A caller that says it was on a call: it is given its line back, or
/// the call back, or is told why not.
async fn comes_back(
    calls: Arc<Calls>,
    caller: Caller,
    resume: &Resume,
    from: SocketAddr,
) -> Option<Outcome> {
    let position = Position {
        life: resume.life,
        seen: resume.seen,
    };
    let admitted = match resume.token.parse::<ResumeToken>() {
        Ok(token) => match calls.stack.stack().kept.admits(&token).await {
            Ok(true) => Ok(token.session()),
            Ok(false) => Err((
                ErrorCode::Token,
                "the token is not the one its call gave last",
            )),
            Err(Failure(failure)) => {
                eprintln!("waveport: {failure}");
                Err((
                    ErrorCode::Internal,
                    "the calls that are kept cannot be read",
                ))
            }
        },
        Err(_) => Err((ErrorCode::Token, "the token is not one")),
    };
    let id = match admitted {
        Ok(id) => id,
        Err((code, why)) => {
            caller.refuse(code, why).await;
            return None;
        }
    };

    // A call that is under way is given its line back.
    let live = calls.live().get(&id).map(|live| Live {
        handle: live.handle.clone(),
        ..*live
    });
    if let Some(live) = live {
        let asked = asked(&calls, &caller);
        let welcome = calls.welcome(id, &live, asked, Some(resume)).await;
        match live.handle.attach(caller, welcome).await {
            Ok(link) => {
                println!("[{}] the caller is back, from {from}", live.number);
                tokio::spawn(tells(
                    Arc::clone(&calls),
                    id,
                    live.life,
                    asked,
                    link,
                    Some(position),
                ));
            }
            Err(unattached) => unattached.refuse().await,
        }
        return None;
    }
    if !calls.resume {
        let why = "the call is not under way here, and calls that broke off are not gone on with";
        caller.refuse(ErrorCode::NoSession, why).await;
        return None;
    }
    // One that broke off is gone on with, from what was kept of it.
    let Some(number) = calls.take() else {
        caller
            .refuse(ErrorCode::Busy, "no more calls are taken")
            .await;
        return None;
    };
    run(calls, caller, id, number, Some(resume), from).await
}

/// What a caller asked to be told beyond the facts.
fn asked(calls: &Calls, caller: &Caller) -> Asked {
    let wanted = caller.hello().features.iter().map(String::as_str);
    Asked::of(wanted, calls.tool_text)
}

/// Runs the call `id` for `caller` to its end: a new one, or one that
/// broke off and is gone on with, whose caller says with `back` where
/// it was.
async fn run(
    calls: Arc<Calls>,
    caller: Caller,
    id: SessionId,
    number: u32,
    back: Option<&Resume>,
    from: SocketAddr,
) -> Option<Outcome> {
    let (transport, handle) = caller.transport(CALLER);
    let (lines, mut printed) = mpsc::unbounded_channel();
    let tell = Box::new(move |line: String| {
        let _ = lines.send(line);
    });
    let resumed = back.is_some();
    let started = calls
        .stack
        .start(id, Box::new(transport), tell, resumed, None)
        .await;
    let call = match started {
        Ok(call) => call,
        Err(Failure(failure)) => {
            eprintln!("waveport: {failure}");
            caller
                .refuse(ErrorCode::Internal, "the call could not be started")
                .await;
            return Some(Outcome::Broken);
        }
    };
    let entry = Live {
        handle: handle.clone(),
        number,
        life: call.life(),
        grace_ms: u32::try_from(call.grace.as_millis()).unwrap_or(u32::MAX),
    };
    let asked = asked(&calls, &caller);
    let welcome = calls.welcome(id, &entry, asked, back).await;
    let life = entry.life;
    let back = back.map(|resume| Position {
        life: resume.life,
        seen: resume.seen,
    });
    calls.live().insert(id, entry);
    println!("[{number}] {}, from {from}", call::begins(&call));
    for note in &call.notes {
        println!("[{number}] note: {note}");
    }
    match handle.attach(caller, welcome).await {
        Ok(link) => {
            tokio::spawn(tells(Arc::clone(&calls), id, life, asked, link, back));
        }
        // The call is over already.
        Err(unattached) => unattached.refuse().await,
    }

    let exit = call.exit();
    tokio::pin!(exit);
    let exit = loop {
        tokio::select! {
            exit = &mut exit => break exit,
            Some(said) = printed.recv() => println!("[{number}] {said}"),
        }
    };
    while let Ok(said) = printed.try_recv() {
        println!("[{number}] {said}");
    }
    calls.live().remove(&id);
    calls.stack.settle(id, &exit, None).await;
    Some(call::outcome(&exit))
}

/// Tells the caller on one connection of the call `id`, which is in the
/// life `life`, what the call does, and does what the caller asks that
/// its line cannot. `back` is where a caller that came back was.
///
/// It ends with the connection, or with the call: the caller is then
/// told goodbye, after everything it was told here.
async fn tells(
    calls: Arc<Calls>,
    id: SessionId,
    life: u64,
    asked: Asked,
    mut link: Link,
    back: Option<Position>,
) {
    let stack = calls.stack.stack();
    let mut follower = Follower::begin(stack, id, life, asked, None, back);
    loop {
        tokio::select! {
            next = follower.next(stack) => {
                let Some((message, _)) = next else {
                    return;
                };
                // How the call ended is the last thing there is to tell.
                let last = shown::closes(&message);
                if link.tell(message).await.is_err() || last {
                    return;
                }
            }
            wanted = link.asked() => match wanted {
                Some(FromClient::Interrupt) => {
                    let _ = stack.ask(id, SessionInput::Interrupt);
                }
                Some(_) => {}
                None => return,
            },
        }
    }
}
