//! Calls taken over a WebSocket: the agent is here, and each caller is a
//! `waveport client` somewhere else.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::mpsc;
use tokio::task::JoinSet;
use waveport_domain::ids::ParticipantId;
use waveport_ports::journal::SessionId;
use waveport_transport_websocket::{GREETING_TIMEOUT, Knock, Listener};

use crate::args::Serve;
use crate::call::{self, Outcome, Setup};
use crate::{Context, Failure};

/// What the calls of one server share.
struct Calls {
    stack: Setup,
    /// How many calls may be taken in all, if that is limited.
    at_most: Option<u32>,
    /// How many have been.
    taken: AtomicU32,
    /// The calls that are under way.
    live: Mutex<Vec<SessionId>>,
}

impl Calls {
    fn live(&self) -> std::sync::MutexGuard<'_, Vec<SessionId>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether as many calls have been taken as may be.
    fn full(&self) -> bool {
        self.at_most
            .is_some_and(|most| self.taken.load(Ordering::Relaxed) >= most)
    }
}

/// Takes calls until it is told to stop, or has taken as many as it was
/// told to.
pub(crate) async fn serve(args: Serve) -> Result<Outcome, Failure> {
    let stack = Setup::build(&args.agent, Some(&args.aloud), false).await?;
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
    });

    let mut running = JoinSet::new();
    let mut ending = false;
    let mut went = Outcome::Ended;
    loop {
        if (ending || calls.full()) && running.is_empty() {
            break;
        }
        tokio::select! {
            knock = listener.accept(), if !ending && !calls.full() => match knock {
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
                for id in calls.live().iter() {
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
/// room for one. Returns how the call went, or nothing if there was none.
async fn one(calls: Arc<Calls>, knock: Knock) -> Option<Outcome> {
    let from = knock.from();
    let caller = match knock.greet(GREETING_TIMEOUT).await {
        Ok(caller) => caller,
        Err(error) => {
            eprintln!("waveport: a connection from {from} was not a call: {error}");
            return None;
        }
    };
    // Two that say hello at once may both have found room. Only as many
    // as may be are taken.
    let number = calls.taken.fetch_add(1, Ordering::Relaxed) + 1;
    if calls.at_most.is_some_and(|most| number > most) {
        caller.turn_away("no more calls are taken").await;
        return None;
    }

    let id = call::name();
    let (transport, far_end) = caller.into_transport(&id.to_string(), ParticipantId(0));
    let (lines, mut printed) = mpsc::unbounded_channel();
    // The caller is told as the call is, from the call's own thread: what
    // is told last, how the call ended, is then on its way before the
    // goodbye.
    let tell = Box::new(move |line: String| {
        far_end.say(line.clone());
        let _ = lines.send(line);
    });
    let call = match calls
        .stack
        .start(id, Box::new(transport), tell, false, None)
        .await
    {
        Ok(call) => call,
        Err(Failure(failure)) => {
            eprintln!("waveport: {failure}");
            return Some(Outcome::Broken);
        }
    };
    calls.live().push(id);
    println!("[{number}] the call has begun, from {from}");
    for note in &call.notes {
        println!("[{number}] note: {note}");
    }
    let exit = call.exit();
    tokio::pin!(exit);
    let exit = loop {
        tokio::select! {
            exit = &mut exit => break exit,
            Some(line) = printed.recv() => println!("[{number}] {line}"),
        }
    };
    while let Ok(line) = printed.try_recv() {
        println!("[{number}] {line}");
    }
    calls.live().retain(|live| *live != id);
    calls.stack.settle(id, &exit, None).await;
    Some(call::outcome(&exit))
}
