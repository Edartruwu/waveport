//! A session on a line: its far end is on a WebSocket.

use core::time::Duration;

use waveport_adapter_kit::ws::{WsConn, WsRequest};
use waveport_protocol::client::v1::{
    AudioFormat, ByeReason, Codec, Cut, ErrorCode, Fact, Lifecycle, News, Resume,
};
use waveport_testkit::servers::HttpServer;
use waveport_transport_websocket::{Client, Heard, Hello, PROTOCOL, Unconnected, Welcome};

use crate::far_end::{Answer, ask};
use crate::{ANSWER, Served, answer, config, desk, slow_answer, stops_short};

/// Where the sessions of a daemon are kept that another is started
/// after.
const KEPT: &str = "[journal]\nfile = \"sessions.redb\"\n";

/// What the agent answers the first question with, in one piece.
const ANSWERED: &str = "It is ten past three. Anything else?";

/// How long anything a daemon is asked may take on a machine that is
/// busy with other tests.
const SOON: Duration = Duration::from_secs(10);

/// The hello of a client that connects with `token`, and writes.
fn hello(token: &str) -> Hello {
    Hello {
        token: Some(token.to_owned()),
        features: vec!["said".to_owned()],
        ..Hello::new(None)
    }
}

/// The hello of a client that comes back, having been welcomed with
/// `welcome` and sent the facts up to `seen`.
fn hello_again(welcome: &Welcome, seen: u64) -> Hello {
    Hello {
        resume: Some(Resume {
            token: welcome.resume_token.clone().unwrap(),
            life: welcome.life,
            seen,
        }),
        features: vec!["said".to_owned()],
        ..Hello::new(None)
    }
}

/// What the client is told until `last`, which is the last of it.
async fn until(client: &mut Client, last: impl Fn(&Heard) -> bool) -> Vec<Heard> {
    let mut heard = Vec::new();
    let hearing = async {
        loop {
            let next = client.next().await.unwrap();
            let next = next.unwrap_or_else(|| panic!("the line closed after {heard:#?}"));
            let done = last(&next);
            heard.push(next);
            if done {
                return;
            }
        }
    };
    let told = tokio::time::timeout(SOON, hearing).await;
    assert!(
        told.is_ok(),
        "the client was not told what it waited for: {heard:#?}"
    );
    heard
}

/// Whether the client was told that the agent said `text`, as a fact.
fn said(text: &'static str) -> impl Fn(&Heard) -> bool {
    move |heard| {
        matches!(heard, Heard::Fact { fact: Fact::UtteranceCommitted { text: said, .. }, .. }
            if said == text)
    }
}

/// The numbers of the facts in `heard`.
fn numbers(heard: &[Heard]) -> Vec<u64> {
    heard
        .iter()
        .filter_map(|heard| match heard {
            Heard::Fact { seq, .. } => Some(*seq),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_client_on_a_line_writes_and_is_answered() {
    let llm = HttpServer::start(vec![answer(&ANSWER, None)]).await;
    let daemon = Served::start(&desk(), &config(&llm, ""), &[]).await;
    let (name, token) = daemon.session().await;
    let address = format!("ws://{}", daemon.address);

    let mut client = Client::connect(&address, &hello(&token), SOON)
        .await
        .unwrap();
    let welcome = client.welcome().clone();
    assert_eq!(
        (welcome.session.as_str(), welcome.life, welcome.resumed),
        (name.as_str(), 1, false)
    );
    assert!(
        welcome
            .resume_token
            .as_deref()
            .unwrap()
            .starts_with("wpr1_")
    );
    assert_eq!(welcome.features, ["said"]);
    // The session waits for a client whose connection drops.
    assert_eq!(welcome.grace_ms, Some(30_000));

    // The agent greets, and answers what is written to it. The client
    // is told as it is written, and as facts, each with its number.
    let greeted = until(&mut client, said("Good morning.")).await;
    client.text("What time is it?").await.unwrap();
    let answered = until(&mut client, said(ANSWERED)).await;
    let all = format!("{greeted:#?}\n{answered:#?}");
    let told: String = answered
        .iter()
        .filter_map(|heard| match heard {
            Heard::Told(News::Said { text, .. }) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(told, ANSWERED, "{all}");
    assert!(
        answered.iter().any(|heard| matches!(heard,
            Heard::Fact { fact: Fact::TurnCommitted { text, .. }, .. } if text == "What time is it?")),
        "{all}"
    );
    let mut seen = numbers(&greeted);
    seen.extend(numbers(&answered));
    let in_order: Vec<u64> = (1..).take(seen.len()).collect();
    assert_eq!(seen, in_order, "{all}");

    // Whoever looks after the session sees that it runs, and how.
    let info = daemon
        .ask("GET", &format!("/v1/sessions/{name}"), None)
        .await
        .json();
    assert_eq!(
        (&info["state"], &info["mode"]),
        (&"running".into(), &"text".into()),
        "{info}"
    );

    // The token let one client in, once.
    let again = Client::connect(&address, &hello(&token), SOON).await;
    let Err(Unconnected::Refused { code, .. }) = again else {
        panic!("a client token was taken twice");
    };
    assert_eq!(code, ErrorCode::Token);

    // The application ends the session. Its client is told how it
    // ended, and then goodbye.
    let ended = daemon
        .ask("DELETE", &format!("/v1/sessions/{name}"), None)
        .await;
    assert_eq!(ended.status, 202, "{ended:?}");
    let last = until(&mut client, |heard| matches!(heard, Heard::Bye(_))).await;
    let closed = last.iter().rev().nth(1);
    assert!(
        matches!(
            closed,
            Some(Heard::Fact {
                fact: Fact::LifecycleChanged {
                    to: Lifecycle::Closed,
                    ..
                },
                ..
            })
        ),
        "{last:#?}"
    );
    assert_eq!(
        last.last(),
        Some(&Heard::Bye(ByeReason::Ended)),
        "{last:#?}"
    );
}

/// The address a client connects to `daemon` at.
fn line_of(daemon: &Served) -> String {
    format!("ws://{}", daemon.address)
}

/// The number of the last fact in `heard`.
fn last_of(heard: &[Heard]) -> u64 {
    *numbers(heard).last().unwrap()
}

/// What a client that is refused is refused with.
fn refused(connected: Result<Client, Unconnected>) -> (ErrorCode, String) {
    match connected {
        Err(Unconnected::Refused { code, message }) => (code, message),
        Err(Unconnected::Failed(error)) => panic!("the client was not answered: {error}"),
        Ok(client) => panic!("the client was taken: {:?}", client.welcome()),
    }
}

/// What the conversation of a session has come to, as a client that
/// comes back is told: what was said, in order.
fn conversation(heard: &[Heard]) -> Vec<String> {
    let state = heard.iter().find_map(|heard| match heard {
        Heard::State(state) => Some(serde_json::to_value(state).unwrap()),
        _ => None,
    });
    let state = state.unwrap_or_else(|| panic!("no state in {heard:#?}"));
    state["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["text"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn a_client_whose_line_drops_comes_back_to_its_session() {
    let llm = HttpServer::start(vec![answer(&ANSWER, None)]).await;
    let daemon = Served::start(&desk(), &config(&llm, ""), &[]).await;
    let (name, token) = daemon.session().await;
    let address = line_of(&daemon);
    let mut client = Client::connect(&address, &hello(&token), SOON)
        .await
        .unwrap();
    let welcome = client.welcome().clone();
    let greeted = until(&mut client, said("Good morning.")).await;
    let seen = last_of(&greeted);
    client.drop_line();

    // The session is not over for that: it waits.
    let info = daemon
        .ask("GET", &format!("/v1/sessions/{name}"), None)
        .await
        .json();
    assert_eq!(info["state"], "running", "{info}");

    // Nobody comes back to it with a token that is not its own: one
    // that is no token, and one that reads as its own and is not.
    let mut other = welcome.clone();
    let token = other.resume_token.as_mut().unwrap();
    let last = token.pop().unwrap();
    token.push(if last == '0' { '1' } else { '0' });
    let stranger = Welcome {
        resume_token: Some("wpr1_madeup".to_owned()),
        ..welcome.clone()
    };
    for forged in [&stranger, &other] {
        let came = Client::connect(&address, &hello_again(forged, seen), SOON).await;
        assert_eq!(refused(came).0, ErrorCode::Token);
    }

    // Its client comes back, says where it was, and is on the line
    // again: in the same life, with the token it has.
    let mut client = Client::connect(&address, &hello_again(&welcome, seen), SOON)
        .await
        .unwrap();
    let back = client.welcome().clone();
    assert!(back.resumed, "{back:?}");
    assert_eq!(
        (&back.session, back.life, &back.resume_token),
        (&welcome.session, 1, &welcome.resume_token)
    );
    client.text("What time is it?").await.unwrap();
    let answered = until(&mut client, said(ANSWERED)).await;
    // It is told what came about since, and nothing twice.
    let told = numbers(&answered);
    let in_order: Vec<u64> = (seen + 1..).take(told.len()).collect();
    assert_eq!(told, in_order, "{answered:#?}");

    // The same client on another connection takes the place of the one
    // it has, which is told so.
    let once_more = hello_again(&welcome, last_of(&answered));
    let again = Client::connect(&address, &once_more, SOON).await.unwrap();
    let parted = until(&mut client, |heard| matches!(heard, Heard::Bye(_))).await;
    assert_eq!(parted.last(), Some(&Heard::Bye(ByeReason::Replaced)));
    // And leaves, which ends the session.
    again.bye().await;
    for _ in 0..500 {
        let gone = daemon
            .ask("GET", &format!("/v1/sessions/{name}"), None)
            .await;
        if gone.status == 404 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the session did not end when its client left");
}

#[tokio::test]
async fn a_client_comes_back_to_the_daemon_that_follows_one_that_was_killed() {
    // The first answer is begun and never finished. The journal takes
    // what waits for it together, so an answer that ends could be
    // stored with the question it answers, however long it took.
    let llm = HttpServer::start(vec![
        stops_short(),
        answer(&["Half past", " three."], None),
        answer(&["You are", " welcome."], None),
    ])
    .await;
    let desk = desk();
    // The daemon's journal stops once it has stored what the client
    // wrote, and everything else goes on: the client is shown the
    // answer as far as it is written, which is stored nowhere when the
    // daemon is killed.
    let place = "after:turn_committed";
    let halt = [("WAVEPORT_TEST_HALT", place)];
    let mut daemon = Served::start(&desk, &config(&llm, KEPT), &halt).await;
    let (name, token) = daemon.session().await;
    let mut client = Client::connect(&line_of(&daemon), &hello(&token), SOON)
        .await
        .unwrap();
    let welcome = client.welcome().clone();
    until(&mut client, said("Good morning.")).await;
    client.text("What time is it?").await.unwrap();
    let begun =
        |heard: &Heard| matches!(heard, Heard::Told(News::Said { text, .. }) if text == ANSWER[0]);
    let seen = last_of(&until(&mut client, begun).await);
    daemon.halted_at(place).await;
    daemon.kill();
    // The line is gone with the daemon, and nobody said goodbye.
    let gone = async {
        loop {
            match client.next().await {
                Ok(Some(Heard::Bye(reason))) => panic!("the daemon said goodbye: {reason:?}"),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return,
            }
        }
    };
    tokio::time::timeout(SOON, gone).await.unwrap();

    // The daemon that is started next has the session, and knows that
    // it was on a line: nobody but its far end goes on with it.
    let daemon = Served::start(&desk, &config(&llm, KEPT), &[]).await;
    let path = format!("/v1/sessions/{name}");
    let info = daemon.ask("GET", &path, None).await.json();
    assert_eq!(info["state"], "broken", "{info}");
    let followed = daemon.follow(&name, "", None, &[]).await.err().unwrap();
    assert_eq!(
        (followed.status, followed.code().as_str()),
        (409, "out_of_place")
    );
    let written = r#"{"type":"text","text":"Anybody?"}"#;
    let typed = daemon
        .ask("POST", &format!("{path}/input"), Some(written))
        .await;
    assert_eq!((typed.status, typed.code().as_str()), (409, "out_of_place"));
    let info = daemon.ask("GET", &path, None).await.json();
    assert_eq!(info["state"], "broken", "{info}");

    // The client comes back on a line, and the session goes on, on it:
    // in its next life, which the client is told of with what the
    // session has come to. What it was shown of an answer is not part
    // of that.
    let mut client = Client::connect(&line_of(&daemon), &hello_again(&welcome, seen), SOON)
        .await
        .unwrap();
    let back = client.welcome().clone();
    assert!(back.resumed, "{back:?}");
    assert_eq!(
        (&back.session, back.life, &back.resume_token),
        (&welcome.session, 2, &welcome.resume_token)
    );
    let stands = until(&mut client, |heard| matches!(heard, Heard::State(_))).await;
    assert!(matches!(stands[0], Heard::Gap { .. }), "{stands:#?}");
    assert_eq!(
        conversation(&stands),
        ["Good morning.", "What time is it?"],
        "{stands:#?}"
    );
    // It is owed an answer, and is given one. Then it writes again.
    until(&mut client, said("Half past three.")).await;
    client.text("And now?").await.unwrap();
    until(&mut client, said("You are welcome.")).await;
    // The model was asked with what the session holds.
    let asked = llm.requests();
    assert_eq!(asked.len(), 3);
    let last = &asked[2].body;
    for earlier in [
        "Good morning.",
        "What time is it?",
        "Half past three.",
        "And now?",
    ] {
        assert!(last.contains(earlier), "{earlier}: {last}");
    }
    assert!(!last.contains(ANSWERED), "{last}");
    let info = daemon.ask("GET", &path, None).await.json();
    assert_eq!(
        (&info["state"], &info["life"]),
        (&"running".into(), &2.into()),
        "{info}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_daemon_that_is_told_to_stop_says_so_on_the_line_and_the_next_gives_it_back() {
    let llm = HttpServer::start(vec![answer(&ANSWER, None)]).await;
    let desk = desk();
    let mut daemon = Served::start(&desk, &config(&llm, KEPT), &[]).await;
    let (_, token) = daemon.session().await;
    let mut client = Client::connect(&line_of(&daemon), &hello(&token), SOON)
        .await
        .unwrap();
    let welcome = client.welcome().clone();
    let seen = last_of(&until(&mut client, said("Good morning.")).await);

    // The client is told that the daemon goes, which is no end of the
    // session, and its line is closed in good order.
    daemon.tell_to_stop();
    let last = until(&mut client, |heard| matches!(heard, Heard::Bye(_))).await;
    assert_eq!(last.last(), Some(&Heard::Bye(ByeReason::GoingAway)));
    let after = tokio::time::timeout(SOON, client.next()).await.unwrap();
    assert!(matches!(after, Ok(None)), "{after:?}");
    let status = daemon.ended().await;
    assert!(status.success(), "{}", daemon.errors());

    // It comes back to the daemon that is started next.
    let daemon = Served::start(&desk, &config(&llm, KEPT), &[]).await;
    let mut client = Client::connect(&line_of(&daemon), &hello_again(&welcome, seen), SOON)
        .await
        .unwrap();
    let back = client.welcome().clone();
    assert_eq!((back.resumed, back.life), (true, 2), "{back:?}");
    client.text("What time is it?").await.unwrap();
    until(&mut client, said(ANSWERED)).await;
}

/// Asks for a line as a browser does, with `more` besides or in place
/// of what one says. A header that is given no value is left out.
pub(crate) async fn asks_for_a_line(daemon: &Served, path: &str, more: &[(&str, &str)]) -> Answer {
    let mut headers = vec![
        ("Upgrade", "websocket"),
        ("Connection", "Upgrade"),
        ("Sec-WebSocket-Version", "13"),
        ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ("Sec-WebSocket-Protocol", PROTOCOL),
    ];
    for (name, value) in more {
        headers.retain(|(named, _)| named != name);
        if !value.is_empty() {
            headers.push((name, value));
        }
    }
    ask(&daemon.address, "GET", path, &headers, None).await
}

#[tokio::test]
async fn what_does_not_ask_as_a_client_asks_gets_no_line() {
    let llm = HttpServer::start(Vec::new()).await;
    let app = "https://app.example";
    let more = format!("allowed_origins = [\"{app}\"]");
    let daemon = Served::start(&desk(), &config(&llm, &more), &[]).await;
    let (_, token) = daemon.session().await;

    // A request that is for no WebSocket, or for one of another kind,
    // is told what a line is.
    let plain = ask(&daemon.address, "GET", "/v1/client", &[], None).await;
    assert_eq!((plain.status, plain.code().as_str()), (426, "unsupported"));
    assert_eq!(plain.header("upgrade"), Some("websocket"));
    let old = asks_for_a_line(&daemon, "/v1/client", &[("Sec-WebSocket-Version", "8")]).await;
    assert_eq!((old.status, old.code().as_str()), (426, "version"));
    assert_eq!(old.header("sec-websocket-version"), Some("13"));
    let other = [("Sec-WebSocket-Protocol", "waveport.ws.v0, mqtt")];
    let other = asks_for_a_line(&daemon, "/v1/client", &other).await;
    assert_eq!((other.status, other.code().as_str()), (400, "version"));
    let keyless = asks_for_a_line(&daemon, "/v1/client", &[("Sec-WebSocket-Key", "")]).await;
    assert_eq!(
        (keyless.status, keyless.code().as_str()),
        (400, "malformed")
    );

    // Nothing is said in the address: a token that is written there is
    // written down by whatever the request passes through.
    let path = format!("/v1/client?token={token}");
    let named = asks_for_a_line(&daemon, &path, &[]).await;
    assert_eq!((named.status, named.code().as_str()), (400, "malformed"));

    // A page from elsewhere gets no line, and one the daemon was told
    // of does.
    let elsewhere = [("Origin", "https://evil.example")];
    let stolen = asks_for_a_line(&daemon, "/v1/client", &elsewhere).await;
    assert_eq!((stolen.status, stolen.code().as_str()), (403, "forbidden"));
    let url = format!("ws://{}/v1/client", daemon.address);
    let from_app = WsRequest {
        url: &url,
        headers: &[("Sec-WebSocket-Protocol", PROTOCOL), ("Origin", app)],
        timeout: SOON,
    };
    WsConn::connect(from_app).await.unwrap().close().await;

    // On a line, whoever says nothing of a session, or what is no token
    // of one, is told so. The token of before is still good: none of
    // this used it.
    let address = line_of(&daemon);
    let nobody = Client::connect(&address, &Hello::new(None), SOON).await;
    assert_eq!(refused(nobody).0, ErrorCode::Token);
    let made_up = Client::connect(&address, &hello("wpc1_madeup"), SOON).await;
    assert_eq!(refused(made_up).0, ErrorCode::Token);
    let client = Client::connect(&address, &hello(&token), SOON)
        .await
        .unwrap();
    client.bye().await;
}

#[tokio::test]
async fn a_session_that_is_written_to_is_on_no_line() {
    let llm = HttpServer::start(Vec::new()).await;
    let daemon = Served::start(&desk(), &config(&llm, ""), &[]).await;
    // Its far end follows it as events, which starts it on no line.
    let (name, token) = daemon.session().await;
    let mut events = daemon.follow(&name, "", Some(&token), &[]).await.unwrap();
    events.next().await.unwrap();
    // Another far end that comes for it on a line is told where it is.
    let minted = daemon
        .ask("POST", &format!("/v1/sessions/{name}/tokens"), None)
        .await
        .json();
    let another = minted["client_token"].as_str().unwrap();
    let came = Client::connect(&line_of(&daemon), &hello(another), SOON).await;
    let (code, why) = refused(came);
    assert_eq!(code, ErrorCode::Unsupported, "{why}");
    assert!(why.contains("on no line"), "{why}");
}

#[tokio::test]
async fn an_agent_that_only_writes_is_not_spoken_to() {
    let llm = HttpServer::start(Vec::new()).await;
    let daemon = Served::start(&desk(), &config(&llm, ""), &[]).await;
    let (name, token) = daemon.session().await;
    let aloud = Hello {
        token: Some(token),
        ..Hello::new(Some(AudioFormat {
            codec: Codec::Pcm16,
            rate: 16_000,
        }))
    };
    let came = Client::connect(&line_of(&daemon), &aloud, SOON).await;
    let (code, why) = refused(came);
    assert_eq!(code, ErrorCode::Unsupported, "{why}\n{}", daemon.errors());
    assert!(why.contains("neither hears nor speaks"), "{why}");
    // The session was not started for that, and is there for a client
    // that writes.
    let path = format!("/v1/sessions/{name}");
    let info = daemon.ask("GET", &path, None).await.json();
    assert_eq!(info["state"], "waiting", "{info}");
    let minted = daemon
        .ask("POST", &format!("{path}/tokens"), None)
        .await
        .json();
    let another = minted["client_token"].as_str().unwrap();
    let mut client = Client::connect(&line_of(&daemon), &hello(another), SOON)
        .await
        .unwrap();
    until(&mut client, said("Good morning.")).await;
}

#[tokio::test]
async fn a_client_on_a_line_is_told_of_the_tools_and_not_what_they_said() {
    let script = vec![answer(&[], Some("get_time")), answer(&ANSWER, None)];
    let llm = HttpServer::start(script).await;
    // Whoever holds a control key may be shown it. A client is not.
    let shown = "[control]\nshow_tool_text = true\n";
    let daemon = Served::start(&desk(), &config(&llm, shown), &[]).await;
    let (_, token) = daemon.session().await;
    let asks = Hello {
        features: vec!["tools".to_owned(), "smells".to_owned()],
        ..hello(&token)
    };
    let mut client = Client::connect(&line_of(&daemon), &asks, SOON)
        .await
        .unwrap();
    // It is told what it asked for that there is.
    assert_eq!(client.welcome().features, ["tools"]);
    client.text("What time is it?").await.unwrap();
    let answered = until(&mut client, said(ANSWERED)).await;
    let called = answered.iter().find_map(|heard| match heard {
        Heard::Fact {
            fact: Fact::ToolCalled { tool, args, .. },
            ..
        } => Some((tool.as_str(), args.clone())),
        _ => None,
    });
    assert_eq!(called, Some(("get_time", None)), "{answered:#?}");
    let finished = answered.iter().find_map(|heard| match heard {
        Heard::Fact {
            fact: Fact::ToolFinished { result, .. },
            ..
        } => Some(result.clone()),
        _ => None,
    });
    assert_eq!(finished, Some(None), "{answered:#?}");
    // It did not ask to be told what the agent writes as it is written.
    assert!(
        !answered
            .iter()
            .any(|heard| matches!(heard, Heard::Told(News::Said { .. }))),
        "{answered:#?}"
    );
}

#[tokio::test]
async fn a_client_on_a_line_stops_the_agent() {
    // An answer that takes a while to be written.
    let slowly = slow_answer(&ANSWER, Duration::from_millis(150));
    let llm = HttpServer::start(vec![slowly]).await;
    let daemon = Served::start(&desk(), &config(&llm, ""), &[]).await;
    let (_, token) = daemon.session().await;
    let mut client = Client::connect(&line_of(&daemon), &hello(&token), SOON)
        .await
        .unwrap();
    until(&mut client, said("Good morning.")).await;
    client.text("What time is it?").await.unwrap();
    // It is under way when the client says to stop.
    let begun = |heard: &Heard| matches!(heard, Heard::Told(News::Said { .. }));
    until(&mut client, begun).await;
    client.interrupt().await.unwrap();
    let cut = |heard: &Heard| {
        matches!(
            heard,
            Heard::Fact {
                fact: Fact::CutOff {
                    cause: Cut::Interrupted
                },
                ..
            }
        )
    };
    let stopped = until(&mut client, cut).await;
    // What was written of the answer after that is told to nobody.
    assert!(!stopped.iter().any(said(ANSWERED)), "{stopped:#?}");
}

#[tokio::test]
async fn a_session_that_was_written_to_is_gone_on_with_on_no_line() {
    let llm = HttpServer::start(vec![answer(&ANSWER, None)]).await;
    let desk = desk();
    let mut daemon = Served::start(&desk, &config(&llm, KEPT), &[]).await;
    // Its far end follows it as events, and is given a token to come
    // back with.
    let (name, token) = daemon.session().await;
    let mut events = daemon.follow(&name, "", Some(&token), &[]).await.unwrap();
    let welcome = events.next().await.unwrap().json();
    let resume = welcome["resume_token"].as_str().unwrap().to_owned();
    events
        .until(|event| crate::is_fact(event, "utterance_committed"))
        .await;
    daemon.kill();
    drop(events);

    // With the daemon that is started next, the token is no way onto a
    // line: the session was on none, and stays on none.
    let daemon = Served::start(&desk, &config(&llm, KEPT), &[]).await;
    let back = Welcome {
        resume_token: Some(resume.clone()),
        life: 1,
        ..Welcome::default()
    };
    let came = Client::connect(&line_of(&daemon), &hello_again(&back, 1), SOON).await;
    let (code, why) = refused(came);
    assert_eq!(code, ErrorCode::Unsupported, "{why}");
    let info = daemon
        .ask("GET", &format!("/v1/sessions/{name}"), None)
        .await
        .json();
    assert_eq!(info["state"], "broken", "{info}");
    // It is gone on with the way it was had.
    let mut events = daemon.follow(&name, "", Some(&resume), &[]).await.unwrap();
    assert_eq!(events.next().await.unwrap().json()["life"], 2);
}
