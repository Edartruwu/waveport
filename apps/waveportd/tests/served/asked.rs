//! What the daemon is asked, and what it answers.

use core::time::Duration;

use serde_json::{Value, json};
use waveport_protocol::client::v1::View;
use waveport_testkit::servers::HttpServer;

use crate::far_end::{Event, ask};
use crate::{ANSWER, Served, answer, config, desk, is_fact, refused_to_start, viewed};

/// The facts among `events`, each as its number and its kind.
fn facts(events: &[Event]) -> Vec<(u64, String)> {
    events
        .iter()
        .filter(|event| event.kind == "fact")
        .map(|event| {
            let fact = event.json();
            let kind = fact["kind"].as_str().unwrap().to_owned();
            (fact["seq"].as_u64().unwrap(), kind)
        })
        .collect()
}

/// What `events` tell of the text of the response `generation` as it
/// went out.
fn said(events: &[Event], generation: u64) -> String {
    events
        .iter()
        .filter(|event| event.kind == "told")
        .map(Event::json)
        .filter(|news| news["kind"] == "said" && news["generation"] == generation)
        .map(|news| news["text"].as_str().unwrap().to_owned())
        .collect()
}

/// Whether the event is the news that the agent has nothing more to do
/// for the response `generation`: what follows the fact that ends it.
fn rests(event: &Event, generation: u64) -> bool {
    let news = event.json();
    event.kind == "told" && news["activity"] == "listening" && news["generation"] == generation
}

/// Whether the event is the fact that ends the response `generation`.
fn ends(event: &Event, generation: u64) -> bool {
    is_fact(event, "utterance_committed") && event.json()["generation"] == generation
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one session from its beginning to its end, told in the order it happens"
)]
async fn a_chat_is_had_over_http_from_its_first_word_to_its_last() {
    let llm = HttpServer::start(vec![answer(&ANSWER, None)]).await;
    let daemon = Served::start(&desk(), &config(&llm, ""), &[]).await;

    // An application makes a session, and is given what its far end
    // connects with.
    let made = daemon
        .ask("POST", "/v1/sessions", Some(r#"{"agent":"desk"}"#))
        .await;
    assert_eq!(made.status, 201, "{made:?}");
    assert_eq!(made.header("content-type"), Some("application/json"));
    assert_eq!(made.header("cache-control"), Some("no-store"));
    let created = made.json();
    let name = created["session"].as_str().unwrap();
    let token = created["client_token"].as_str().unwrap();
    assert!(name.len() == 32 && name.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(
        token.starts_with("wpc1_") && token.len() == 69,
        "{}",
        token.len()
    );
    assert_eq!(created["connect_within_ms"], 60_000);
    let at = format!("/v1/sessions/{name}");
    assert_eq!(made.header("location"), Some(at.as_str()));
    // Nothing of it runs until somebody comes for it.
    let waits = daemon.ask("GET", &at, None).await.json();
    assert_eq!(
        waits,
        json!({"session": name, "agent": "desk", "state": "waiting"})
    );

    // The far end connects, and is told what the session does from its
    // beginning.
    let mut events = daemon
        .follow(name, "?features=said", Some(token), &[])
        .await
        .unwrap();
    assert_eq!(events.header("content-type"), Some("text/event-stream"));
    assert_eq!(events.header("cache-control"), Some("no-cache"));
    assert_eq!(events.header("x-accel-buffering"), Some("no"));
    let welcome = events.next().await.unwrap();
    assert_eq!(
        (welcome.kind.as_str(), welcome.id.as_deref()),
        ("welcome", None)
    );
    assert_eq!(welcome.retry, Some(2_000));
    let mut welcomed = welcome.json();
    let resume = welcomed["resume_token"].take();
    let resume = resume.as_str().unwrap();
    assert!(resume.starts_with(&format!("wpr1_{name}")) && resume.len() == 69);
    // The session has begun by now, and may have said its greeting: a
    // fact that follows the welcome under a lower number than it names
    // came about before it.
    let next = welcomed["next_seq"].take();
    assert!(next == 1 || next == 2, "{next}");
    assert_eq!(
        welcomed,
        json!({
            "type": "welcome", "v": 1, "session": name, "life": 1, "resumed": false,
            "resume_token": null, "features": ["said"], "next_seq": null,
        })
    );
    let greeted = events.until(|event| ends(event, 1)).await;
    assert_eq!(said(&greeted, 1), "Good morning.");
    let greeting = greeted.last().unwrap();
    assert_eq!(greeting.id.as_deref(), Some("1.1"));
    assert_eq!(greeting.json()["text"], "Good morning.");

    // A client token connects once.
    let again = daemon
        .follow(name, "", Some(token), &[])
        .await
        .err()
        .unwrap();
    assert_eq!((again.status, again.code().as_str()), (401, "token"));
    assert_eq!(again.header("www-authenticate"), Some("Bearer"));

    // What is typed is answered, and the answer is told as it is
    // written.
    daemon.types(name, resume, "What time is it?").await;
    let answered = events.until(|event| ends(event, 2)).await;
    assert_eq!(
        facts(&answered),
        [
            (2, "participant_joined".to_owned()),
            (3, "turn_committed".to_owned()),
            (4, "utterance_committed".to_owned())
        ]
    );
    let whole = ANSWER.concat();
    assert_eq!(said(&answered, 2), whole);
    let turn = answered
        .iter()
        .find(|event| is_fact(event, "turn_committed"));
    assert_eq!(turn.unwrap().json()["text"], "What time is it?");
    let last = answered.last().unwrap();
    assert_eq!(last.id.as_deref(), Some("1.4"));
    assert_eq!(last.json()["text"], whole.as_str());
    assert_eq!(last.json()["truncated"], false);
    // The model was asked once, with what had been said.
    let asked = llm.requests();
    assert_eq!(asked.len(), 1);
    assert!(asked[0].body.contains("Good morning.") && asked[0].body.contains("What time is it?"));

    let runs = daemon.ask("GET", &at, None).await.json();
    let running = json!({
        "session": name, "agent": "desk", "state": "running", "mode": "text", "life": 1,
        "next_seq": 5,
    });
    assert_eq!(runs, running);
    let all = daemon.ask("GET", "/v1/sessions", None).await.json();
    assert_eq!(all, json!({"sessions": [running]}));

    // Whoever comes later is told the same, from the facts: what a
    // client holds of the session does not depend on when it came.
    let mut later = daemon.follow(name, "", Some(resume), &[]).await.unwrap();
    let welcomed = later.next().await.unwrap().json();
    assert_eq!(
        welcomed,
        json!({
            "type": "welcome", "v": 1, "session": name, "life": 1, "resumed": false,
            "next_seq": 5,
        })
    );
    let caught_up = later.until(|event| ends(event, 2)).await;
    // News is of the moment, and is not told again.
    assert!(
        caught_up.iter().all(|event| event.kind == "fact"),
        "{caught_up:#?}"
    );
    let first = viewed(viewed(View::new(), &greeted), &answered);
    assert_eq!(viewed(View::new(), &caught_up), first);
    assert_eq!(first.seen(), 4);

    // The far end leaves, which ends the session: each stream is told
    // so, and ends.
    let path = format!("{at}/input");
    let left = daemon
        .ask_as(resume, "POST", &path, Some(r#"{"type":"bye"}"#))
        .await;
    assert_eq!(left.status, 202, "{left:?}");
    for stream in [&mut events, &mut later] {
        let rest = stream.rest().await;
        let last = rest.last().unwrap();
        assert_eq!(
            last.json(),
            json!({"type": "bye", "reason": "ended"}),
            "{rest:#?}"
        );
        let closed = &rest[rest.len() - 2];
        assert!(is_fact(closed, "lifecycle_changed"), "{rest:#?}");
        assert_eq!(closed.json()["to"], "closed");
    }
    // Nothing is left of it, and its token is a token of nothing.
    let mut gone = daemon.ask("GET", &at, None).await;
    for _ in 0..100 {
        if gone.status == 404 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        gone = daemon.ask("GET", &at, None).await;
    }
    assert_eq!((gone.status, gone.code().as_str()), (404, "no_session"));
    let none = daemon
        .follow(name, "", Some(resume), &[])
        .await
        .err()
        .unwrap();
    assert_eq!((none.status, none.code().as_str()), (401, "token"));
    let all = daemon.ask("GET", "/v1/sessions", None).await.json();
    assert_eq!(all, json!({"sessions": []}));
    let warned =
        "waveportd: there is no control key: whoever is on this machine may ask everything";
    assert_eq!(daemon.noted(warned).await, warned);
}

#[tokio::test]
async fn a_client_that_comes_back_is_told_what_it_missed() {
    let llm = HttpServer::start(vec![answer(&ANSWER, None)]).await;
    let daemon = Served::start(&desk(), &config(&llm, ""), &[]).await;
    let (name, token) = daemon.session().await;
    let mut events = daemon.follow(&name, "", Some(&token), &[]).await.unwrap();
    let welcome = events.next().await.unwrap().json();
    let resume = welcome["resume_token"].as_str().unwrap().to_owned();
    let before = events.until(|event| ends(event, 1)).await;
    // The connection drops. The session goes on: an application types
    // into it, and it answers.
    drop(events);
    let mut all = daemon.follow(&name, "", None, &[]).await.unwrap();
    let path = format!("/v1/sessions/{name}/input");
    let typed = r#"{"type":"text","text":"What time is it?"}"#;
    assert_eq!(daemon.ask("POST", &path, Some(typed)).await.status, 202);
    let all = all.until(|event| ends(event, 2)).await;
    let whole = viewed(View::new(), &all);

    // The client says the last event it was sent, as a browser does by
    // itself, and is sent the ones after it.
    let last = [("Last-Event-ID", "1.1")];
    let mut back = daemon
        .follow(&name, "", Some(&resume), &last)
        .await
        .unwrap();
    let welcomed = back.next().await.unwrap().json();
    assert_eq!(
        welcomed,
        json!({
            "type": "welcome", "v": 1, "session": name, "life": 1, "resumed": true,
            "next_seq": 5,
        })
    );
    let missed = back.until(|event| ends(event, 2)).await;
    let ids: Vec<_> = missed.iter().map(|event| event.id.as_deref()).collect();
    assert_eq!(ids, [Some("1.2"), Some("1.3"), Some("1.4")]);
    assert_eq!(back.idles_for(Duration::from_millis(200)).await, 0);
    assert_eq!(viewed(viewed(View::new(), &before), &missed), whole);

    // The address can say it too.
    let mut back = daemon
        .follow(&name, "?after=1.3", Some(&resume), &[])
        .await
        .unwrap();
    let rest = back.until(|event| event.kind == "fact").await;
    assert_eq!(rest[0].json()["resumed"], true);
    assert_eq!(rest[1].id.as_deref(), Some("1.4"));

    // What a client was told of another life of the session, this life
    // may never have done. And a fact this life has not made, nobody
    // told. Neither can be gone on from: the client is told what the
    // session has come to, which takes the place of what it holds.
    for elsewhere in ["7.1", "1.99"] {
        let last = [("Last-Event-ID", elsewhere)];
        let mut back = daemon
            .follow(&name, "", Some(&resume), &last)
            .await
            .unwrap();
        let told = back.until(|event| event.kind == "state").await;
        let kinds: Vec<_> = told.iter().map(|event| event.kind.as_str()).collect();
        assert_eq!(kinds, ["welcome", "gap", "state"], "{elsewhere}");
        assert_eq!(told[1].json(), json!({"type": "gap", "resumes": 5}));
        assert_eq!(
            (told[1].id.as_deref(), told[2].id.as_deref()),
            (None, Some("1.4"))
        );
        // It held something, which was of somewhere else.
        let held = viewed(View::new(), &before);
        assert_eq!(viewed(held, &told), whole, "{elsewhere}");
    }
}

#[tokio::test]
async fn a_client_that_was_away_too_long_is_told_what_the_session_has_come_to() {
    let llm = HttpServer::start(vec![answer(&ANSWER, None)]).await;
    let daemon = Served::start(&desk(), &config(&llm, ""), &[]).await;
    let (name, token) = daemon.session().await;
    let mut events = daemon.follow(&name, "", Some(&token), &[]).await.unwrap();
    let welcome = events.next().await.unwrap().json();
    let resume = welcome["resume_token"].as_str().unwrap().to_owned();
    let before = events.until(|event| ends(event, 1)).await;
    drop(events);

    // More happens than a session holds for whoever comes back: an
    // application has the agent say a line, and another, and another.
    let mut all = daemon.follow(&name, "", None, &[]).await.unwrap();
    let mut told = all.until(|event| ends(event, 1)).await;
    let path = format!("/v1/sessions/{name}/input");
    let lines = 140;
    for line in 0..lines {
        let say = json!({"type": "say", "text": format!("Line {line}.")}).to_string();
        assert_eq!(daemon.ask("POST", &path, Some(&say)).await.status, 202);
        told.extend(all.until(|event| ends(event, line + 2)).await);
    }
    let made = lines + 1;
    assert_eq!(facts(&told).len() as u64, made);

    // The client that comes back cannot be sent what it missed. It is
    // told so, and what the session has come to.
    let last = [("Last-Event-ID", "1.1")];
    let mut back = daemon
        .follow(&name, "", Some(&resume), &last)
        .await
        .unwrap();
    let caught_up = back.until(|event| event.kind == "state").await;
    let kinds: Vec<_> = caught_up.iter().map(|event| event.kind.as_str()).collect();
    assert_eq!(kinds, ["welcome", "gap", "state"]);
    assert_eq!(caught_up[0].json()["next_seq"], made + 1);
    assert_eq!(caught_up[1].json()["resumes"], made + 1);
    let position = format!("1.{made}");
    assert_eq!(caught_up[2].id.as_deref(), Some(position.as_str()));
    let state = caught_up[2].json();
    assert_eq!(state["upto"], made);
    assert_eq!(state["items"].as_array().unwrap().len() as u64, made);
    // From there it goes on as the others do.
    let say = r#"{"type":"say","text":"One more."}"#;
    assert_eq!(daemon.ask("POST", &path, Some(say)).await.status, 202);
    let one_more = back.until(|event| ends(event, lines + 2)).await;
    assert_eq!(
        facts(&one_more),
        [(made + 1, "utterance_committed".to_owned())]
    );
    told.extend(all.until(|event| ends(event, lines + 2)).await);
    let held = viewed(viewed(View::new(), &before), &caught_up);
    assert_eq!(viewed(held, &one_more), viewed(View::new(), &told));

    // So does whoever comes for the first time, that late.
    let mut late = daemon.follow(&name, "", None, &[]).await.unwrap();
    let caught_up = late.until(|event| event.kind == "state").await;
    assert_eq!(caught_up[2].json()["upto"], made + 1);
    assert_eq!(viewed(View::new(), &caught_up), viewed(View::new(), &told));
    // The model was asked nothing in all of this.
    assert_eq!(llm.requests().len(), 0);
}

#[tokio::test]
async fn a_stream_that_has_nothing_to_say_shows_that_it_is_there() {
    let llm = HttpServer::start(vec![answer(&ANSWER, None)]).await;
    let often = "[limits]\nkeep_alive_ms = 50\n";
    let daemon = Served::start(&desk(), &config(&llm, often), &[]).await;
    let (name, token) = daemon.session().await;
    let mut events = daemon.follow(&name, "", Some(&token), &[]).await.unwrap();
    events.until(|event| rests(event, 1)).await;
    let comments = events.idles_for(Duration::from_millis(600)).await;
    assert!((2..=14).contains(&comments), "{comments}");
}

/// Two control keys that were made up for these tests.
const KEYS: [(&str, &str); 2] = [
    ("WAVEPORTD_KEY", "made-up-control-key-of-a-test"),
    ("WAVEPORTD_KEY_NEXT", "another-made-up-key-of-a-test"),
];

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one session from its beginning to its end, told in the order it happens"
)]
async fn who_may_ask_is_decided_by_what_they_hold() {
    let llm = HttpServer::start(vec![answer(&ANSWER, None)]).await;
    let keyed = "[control]\nkey_env = [\"WAVEPORTD_KEY\", \"WAVEPORTD_KEY_NEXT\"]\n";
    let daemon = Served::start(&desk(), &config(&llm, keyed), &KEYS).await;
    assert_eq!(daemon.errors(), "");
    let make = Some(r#"{"agent":"desk"}"#);
    let refused = |answer: crate::far_end::Answer| {
        assert_eq!(
            answer.header("www-authenticate"),
            Some("Bearer"),
            "{answer:?}"
        );
        assert_eq!((answer.status, answer.code().as_str()), (401, "token"));
    };

    // Whoever says nothing of who they are, or something that is no
    // key, is asked for one.
    refused(daemon.ask("POST", "/v1/sessions", make).await);
    refused(daemon.ask("GET", "/v1/sessions", None).await);
    for wrong in [
        "made-up-control-key-of-a-tes",
        "x",
        "wpc1_madeup",
        "wpr1_madeup",
    ] {
        refused(daemon.ask_as(wrong, "POST", "/v1/sessions", make).await);
    }
    let basic = [("Authorization", "Basic bWFkZS11cDptYWRlLXVw")];
    refused(ask(&daemon.address, "GET", "/v1/sessions", &basic, None).await);
    // Whether the daemon is there is told to anyone.
    let health = daemon.ask("GET", "/healthz", None).await;
    assert_eq!(
        (health.status, health.json()),
        (200, json!({"status": "ok"}))
    );

    // Either key lets its holder ask everything.
    let (old, new) = (KEYS[0].1, KEYS[1].1);
    let first = daemon.ask_as(old, "POST", "/v1/sessions", make).await;
    let second = daemon.ask_as(new, "POST", "/v1/sessions", make).await;
    assert_eq!((first.status, second.status), (201, 201));
    let (first, second) = (first.json(), second.json());
    let name = first["session"].as_str().unwrap();
    let other = second["session"].as_str().unwrap();
    let token = first["client_token"].as_str().unwrap();
    let all = daemon.ask_as(new, "GET", "/v1/sessions", None).await.json();
    assert_eq!(all["sessions"].as_array().unwrap().len(), 2);

    // A client token connects to its session, and to nothing else: it
    // is not used up by being shown at another door.
    let at = format!("/v1/sessions/{name}");
    let typed = Some(r#"{"type":"text","text":"What time is it?"}"#);
    refused(daemon.ask_as(token, "GET", "/v1/sessions", None).await);
    refused(daemon.ask_as(token, "GET", &at, None).await);
    refused(
        daemon
            .ask_as(token, "POST", &format!("{at}/input"), typed)
            .await,
    );
    refused(
        daemon
            .follow(other, "", Some(token), &[])
            .await
            .err()
            .unwrap(),
    );
    refused(daemon.follow(name, "", None, &[]).await.err().unwrap());
    let mut events = daemon.follow(name, "", Some(token), &[]).await.unwrap();
    let welcome = events.next().await.unwrap().json();
    let resume = welcome["resume_token"].as_str().unwrap().to_owned();
    events.until(|event| ends(event, 1)).await;

    // A resume token follows its session and writes to it, and that is
    // all.
    refused(daemon.ask_as(&resume, "GET", "/v1/sessions", None).await);
    refused(daemon.ask_as(&resume, "GET", &at, None).await);
    refused(daemon.ask_as(&resume, "DELETE", &at, None).await);
    refused(
        daemon
            .ask_as(&resume, "POST", &format!("{at}/tokens"), None)
            .await,
    );
    refused(daemon.ask_as(&resume, "POST", "/v1/sessions", make).await);
    let elsewhere = format!("/v1/sessions/{other}/input");
    refused(daemon.ask_as(&resume, "POST", &elsewhere, typed).await);
    refused(
        daemon
            .follow(other, "", Some(&resume), &[])
            .await
            .err()
            .unwrap(),
    );
    // What only an application asks, a far end does not: to it, it is
    // nothing it knows how to say.
    let say = Some(r#"{"type":"say","text":"Everything is free today."}"#);
    let not_said = daemon
        .ask_as(&resume, "POST", &format!("{at}/input"), say)
        .await;
    assert_eq!(
        (not_said.status, not_said.code().as_str()),
        (400, "unknown_message")
    );
    daemon.types(name, &resume, "What time is it?").await;
    events.until(|event| ends(event, 2)).await;
    drop(daemon.follow(name, "", Some(&resume), &[]).await.unwrap());

    // An application can have another far end connect. The token that
    // one is given takes the place of the one before.
    let minted = daemon
        .ask_as(old, "POST", &format!("{at}/tokens"), None)
        .await;
    assert_eq!(minted.status, 201, "{minted:?}");
    let minted = minted.json();
    assert_eq!(minted["session"], name);
    let mut moved = daemon
        .follow(name, "", minted["client_token"].as_str(), &[])
        .await
        .unwrap();
    let welcome = moved.next().await.unwrap().json();
    let newer = welcome["resume_token"].as_str().unwrap();
    assert_ne!(newer, resume);
    refused(
        daemon
            .follow(name, "", Some(&resume), &[])
            .await
            .err()
            .unwrap(),
    );
    drop(daemon.follow(name, "", Some(newer), &[]).await.unwrap());
    // Whoever holds a control key follows any session.
    drop(daemon.follow(name, "", Some(new), &[]).await.unwrap());
    // No key and no token was said where the daemon is run.
    let errors = daemon.errors();
    for secret in [old, new, token, resume.as_str(), newer] {
        assert!(!errors.contains(secret), "{errors}");
    }
}

#[tokio::test]
async fn a_page_from_elsewhere_is_refused_on_this_machine() {
    let llm = HttpServer::start(vec![answer(&ANSWER, None)]).await;
    let told = "hosts = [\"voice.example\"]\nallowed_origins = [\"https://app.example\"]\n";
    let daemon = Served::start(&desk(), &config(&llm, told), &[]).await;
    let address = daemon.address.as_str();
    let port = address.rsplit_once(':').unwrap().1;
    let list = |headers: Vec<(&'static str, String)>| async move {
        let headers: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        ask(address, "GET", "/v1/sessions", &headers, None).await
    };
    let forbidden = |answer: crate::far_end::Answer| {
        assert_eq!((answer.status, answer.code().as_str()), (403, "forbidden"));
        assert_eq!(answer.header("access-control-allow-origin"), None);
    };

    // A page in a browser on this machine is not this machine: where
    // it comes from is looked at though nothing else is.
    for origin in ["https://evil.example", "http://localhost:3000", "null"] {
        forbidden(list(vec![("Origin", origin.to_owned())]).await);
    }
    // A request a page makes without saying where it comes from, such
    // as for a picture, says so in another way.
    for site in ["cross-site", "same-site"] {
        forbidden(list(vec![("Sec-Fetch-Site", site.to_owned())]).await);
    }
    // A name that somebody pointed at this machine is not the daemon's.
    for host in [format!("evil.example:{port}"), "evil.example".to_owned()] {
        forbidden(list(vec![("Host", host)]).await);
    }

    // The daemon's own pages, and whoever is no page, are let in.
    let own = format!("http://{address}");
    for headers in [
        vec![],
        vec![("Origin", own)],
        vec![("Sec-Fetch-Site", "same-origin".to_owned())],
        vec![("Sec-Fetch-Site", "none".to_owned())],
        vec![("Host", format!("localhost:{port}"))],
        vec![("Host", format!("voice.example:{port}"))],
    ] {
        let answer = list(headers.clone()).await;
        assert_eq!(answer.status, 200, "{headers:?}");
        assert_eq!(answer.header("access-control-allow-origin"), None);
    }

    // A page the daemon was told of is let in, and its browser is told
    // that it may read the answer.
    let app = "https://app.example";
    let answer = list(vec![("Origin", app.to_owned())]).await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.header("access-control-allow-origin"), Some(app));
    assert_eq!(answer.header("vary"), Some("Origin"));
    // Its browser asks first, and is told what the page may ask.
    let first = |origin: &'static str| async move {
        let headers = [
            ("Origin", origin),
            ("Access-Control-Request-Method", "POST"),
            (
                "Access-Control-Request-Headers",
                "authorization,content-type",
            ),
        ];
        ask(address, "OPTIONS", "/v1/sessions", &headers, None).await
    };
    let may = first(app).await;
    assert_eq!((may.status, may.body.as_str()), (204, ""));
    assert_eq!(may.header("access-control-allow-origin"), Some(app));
    assert_eq!(
        may.header("access-control-allow-methods"),
        Some("GET, POST, PATCH, DELETE")
    );
    assert_eq!(
        may.header("access-control-allow-headers"),
        Some("authorization, content-type, if-match, last-event-id")
    );
    forbidden(first("https://evil.example").await);
    // So is a stream of events.
    let (name, token) = daemon.session().await;
    let from_app = [("Origin", app)];
    let events = daemon.follow(&name, "", Some(&token), &from_app).await;
    assert_eq!(
        events.unwrap().header("access-control-allow-origin"),
        Some(app)
    );
    let from_elsewhere = [("Origin", "https://evil.example")];
    let stolen = daemon.follow(&name, "", None, &from_elsewhere).await;
    forbidden(stolen.err().unwrap());

    // Whoever looks after the daemon asks by its address, whatever that
    // is called where they are.
    let health = [("Host", "10.1.2.3:8080")];
    assert_eq!(
        ask(address, "GET", "/healthz", &health, None).await.status,
        200
    );
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one session from its beginning to its end, told in the order it happens"
)]
async fn what_is_asked_wrongly_is_answered_with_a_problem() {
    let llm = HttpServer::start(vec![answer(&ANSWER, None)]).await;
    let small = "[limits]\nbody_bytes = 256\n";
    let daemon = Served::start(&desk(), &config(&llm, small), &[]).await;
    let (name, token) = daemon.session().await;
    let at = format!("/v1/sessions/{name}");
    let mut events = daemon.follow(&name, "", Some(&token), &[]).await.unwrap();
    let welcome = events.next().await.unwrap().json();
    let resume = welcome["resume_token"].as_str().unwrap().to_owned();
    let problem = |answer: crate::far_end::Answer, status: u16, code: &str| {
        assert_eq!(
            (answer.status, answer.code().as_str()),
            (status, code),
            "{answer:?}"
        );
        let problem = answer.json();
        assert!(
            problem["title"].is_string() && problem["detail"].is_string(),
            "{problem}"
        );
    };

    // What is asked for is not there.
    let nobody = Some(r#"{"agent":"nobody"}"#);
    problem(
        daemon.ask("POST", "/v1/sessions", nobody).await,
        404,
        "no_agent",
    );
    let unknown = "/v1/sessions/0000000000000000000000000000002a";
    for path in ["/v1/sessions/zz", unknown] {
        problem(daemon.ask("GET", path, None).await, 404, "no_session");
        problem(daemon.ask("DELETE", path, None).await, 404, "no_session");
    }
    let typed = Some(r#"{"type":"text","text":"Hello?"}"#);
    let nowhere = format!("{unknown}/input");
    problem(daemon.ask("POST", &nowhere, typed).await, 404, "no_session");
    problem(
        daemon.ask("GET", "/v1/nothing", None).await,
        404,
        "unknown_message",
    );
    problem(daemon.ask("GET", "/", None).await, 404, "unknown_message");
    assert_eq!(daemon.ask("PUT", "/v1/sessions", None).await.status, 405);

    // A body is JSON, says so, is of the kind that is asked for, and is
    // no larger than a body may be.
    let address = daemon.address.as_str();
    let make = |body: &'static str, kind: &'static str| async move {
        let headers = [("Content-Type", kind)];
        ask(address, "POST", "/v1/sessions", &headers, Some(body)).await
    };
    let desk_agent = r#"{"agent":"desk"}"#;
    problem(make(desk_agent, "text/plain").await, 415, "unsupported");
    assert_eq!(
        make(desk_agent, "application/json; charset=utf-8")
            .await
            .status,
        201
    );
    problem(
        make("agent=desk", "application/json").await,
        400,
        "malformed",
    );
    problem(
        make(r#"{"agents":"desk"}"#, "application/json").await,
        400,
        "malformed",
    );
    let large = format!(r#"{{"agent":"desk","more":"{}"}}"#, "x".repeat(300));
    let headers = [("Content-Type", "application/json")];
    let too_large = ask(
        &daemon.address,
        "POST",
        "/v1/sessions",
        &headers,
        Some(&large),
    )
    .await;
    problem(too_large, 413, "malformed");

    // What is asked of a session is something a session can do.
    let input = format!("{at}/input");
    for (asked, status, code) in [
        (r#"{"type":"text","text":""}"#, 400, "malformed"),
        (r#"{"type":"press","keys":"12x"}"#, 400, "malformed"),
        (r#"{"type":"dance"}"#, 400, "unknown_message"),
        (r#"{"text":"What time is it?"}"#, 400, "malformed"),
    ] {
        problem(daemon.ask("POST", &input, Some(asked)).await, status, code);
    }
    // And what a far end says here is what is not said with a
    // connection.
    for (said, status, code) in [
        (r#"{"type":"text","text":" "}"#, 400, "malformed"),
        (r#"{"type":"dtmf","keys":"12"}"#, 422, "unsupported"),
        (
            r#"{"type":"played","run":1,"samples":480}"#,
            409,
            "out_of_place",
        ),
        (r#"{"type":"hello","v":1}"#, 409, "out_of_place"),
        (r#"{"type":"hello","v":2}"#, 400, "version"),
    ] {
        problem(
            daemon.ask_as(&resume, "POST", &input, Some(said)).await,
            status,
            code,
        );
    }

    // Where a client was, and how sessions stand, is said as it is
    // written.
    for position in ["yesterday", "1", "1.x", "-1.2"] {
        let last = [("Last-Event-ID", position)];
        let refused = daemon.follow(&name, "", Some(&resume), &last).await;
        problem(refused.err().unwrap(), 400, "malformed");
    }
    let refused = daemon
        .follow(&name, "?after=soon", Some(&resume), &[])
        .await;
    problem(refused.err().unwrap(), 400, "malformed");
    let sideways = daemon.ask("GET", "/v1/sessions?state=sideways", None).await;
    problem(sideways, 400, "malformed");
    let waiting = daemon
        .ask("GET", "/v1/sessions?state=waiting", None)
        .await
        .json();
    let running = daemon
        .ask("GET", "/v1/sessions?state=running", None)
        .await
        .json();
    let count = |sessions: &Value| sessions["sessions"].as_array().unwrap().len();
    assert_eq!((count(&waiting), count(&running)), (1, 1));
    // Nothing that was refused was done.
    events.until(|event| rests(event, 1)).await;
    assert_eq!(events.idles_for(Duration::from_millis(200)).await, 0);
    assert_eq!(llm.requests().len(), 0);
}

#[tokio::test]
async fn a_session_is_ended_by_whoever_made_it() {
    let llm = HttpServer::start(vec![answer(&ANSWER, None)]).await;
    let daemon = Served::start(&desk(), &config(&llm, ""), &[]).await;

    // One that waits is gone at once, and its token with it.
    let (name, token) = daemon.session().await;
    let at = format!("/v1/sessions/{name}");
    let ended = daemon.ask("DELETE", &at, None).await;
    assert_eq!((ended.status, ended.body.as_str()), (204, ""));
    assert_eq!(daemon.ask("GET", &at, None).await.status, 404);
    assert_eq!(daemon.ask("DELETE", &at, None).await.status, 404);
    let late = daemon
        .follow(&name, "", Some(&token), &[])
        .await
        .err()
        .unwrap();
    assert_eq!(late.status, 401);

    // One that runs is asked to end, and whoever follows it is told
    // when it has.
    let (name, token) = daemon.session().await;
    let at = format!("/v1/sessions/{name}");
    let mut events = daemon.follow(&name, "", Some(&token), &[]).await.unwrap();
    events.until(|event| ends(event, 1)).await;
    assert_eq!(daemon.ask("DELETE", &at, None).await.status, 202);
    let rest = events.rest().await;
    assert_eq!(
        rest.last().unwrap().json(),
        json!({"type": "bye", "reason": "ended"})
    );

    let mut all = daemon.ask("GET", "/v1/sessions", None).await.json();
    for _ in 0..100 {
        if all == json!({"sessions": []}) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        all = daemon.ask("GET", "/v1/sessions", None).await.json();
    }
    assert_eq!(all, json!({"sessions": []}));
    // The model was asked nothing: nobody typed.
    assert_eq!(llm.requests().len(), 0);
}

#[tokio::test]
async fn a_session_nobody_comes_for_is_let_go_of() {
    let llm = HttpServer::start(vec![answer(&ANSWER, None)]).await;
    let soon = "[limits]\nconnect_within_ms = 300\n";
    let daemon = Served::start(&desk(), &config(&llm, soon), &[]).await;
    let (name, token) = daemon.session().await;
    let (other, _) = daemon.session().await;
    // Its token is no longer good, and it is gone with it.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let late = daemon
        .follow(&name, "", Some(&token), &[])
        .await
        .err()
        .unwrap();
    assert_eq!((late.status, late.code().as_str()), (401, "token"));
    let at = format!("/v1/sessions/{name}");
    assert_eq!(daemon.ask("GET", &at, None).await.status, 404);
    // So is one that nobody asked about again.
    let all = daemon.ask("GET", "/v1/sessions", None).await.json();
    assert_eq!(all, json!({"sessions": []}));
    let typed = Some(r#"{"type":"text","text":"Hello?"}"#);
    let nowhere = daemon
        .ask("POST", &format!("/v1/sessions/{other}/input"), typed)
        .await;
    assert_eq!(
        (nowhere.status, nowhere.code().as_str()),
        (404, "no_session")
    );
    assert_eq!(llm.requests().len(), 0);
}

#[tokio::test]
async fn tools_are_shown_to_whoever_asks_and_what_they_said_to_whoever_may_see_it() {
    let script = vec![answer(&[], Some("get_time")), answer(&ANSWER, None)];
    let llm = HttpServer::start(script).await;
    let shown = "[control]\nshow_tool_text = true\n";
    let daemon = Served::start(&desk(), &config(&llm, shown), &[]).await;
    let (name, token) = daemon.session().await;
    let mut far_end = daemon
        .follow(&name, "?features=said,tools", Some(&token), &[])
        .await
        .unwrap();
    let welcome = far_end.next().await.unwrap().json();
    assert_eq!(welcome["features"], json!(["said", "tools"]));
    let resume = welcome["resume_token"].as_str().unwrap().to_owned();
    // An application asks for the same, and for something there is not.
    let mut application = daemon
        .follow(&name, "?features=tools&features=smells", None, &[])
        .await
        .unwrap();
    assert_eq!(
        application.next().await.unwrap().json()["features"],
        json!(["tools"])
    );
    let mut plain = daemon.follow(&name, "", None, &[]).await.unwrap();
    assert_eq!(plain.next().await.unwrap().json().get("features"), None);

    daemon.types(&name, &resume, "What time is it?").await;
    let answered = |event: &Event| {
        is_fact(event, "utterance_committed") && event.json()["text"] == ANSWER.concat().as_str()
    };
    let to_far_end = far_end.until(answered).await;
    let to_application = application.until(answered).await;
    let to_plain = plain.until(answered).await;
    let of = |events: &[Event], kind: &str| -> Value {
        let fact = events.iter().find(|event| is_fact(event, kind));
        fact.map_or(Value::Null, Event::json)
    };

    // The far end is told that a tool was called, and how it went.
    let called = of(&to_far_end, "tool_called");
    assert_eq!(called["tool"], "get_time");
    assert_eq!(called.get("args"), None);
    let finished = of(&to_far_end, "tool_finished");
    assert_eq!(finished["outcome"], "completed");
    assert_eq!(finished.get("result"), None);
    // The application is shown what the tool was given and what it
    // answered, because the daemon was told that it may be.
    assert_eq!(of(&to_application, "tool_called")["args"], "{}");
    let result = of(&to_application, "tool_finished")["result"].take();
    assert!(result.as_str().unwrap().contains("\"time\""), "{result}");
    // The far end asked to be told the answer as it is written, and
    // was. Whoever did not ask is told when it is on record.
    let told = |events: &[Event]| -> String {
        let news = events.iter().filter(|event| event.kind == "told");
        let said = news.map(Event::json).filter(|news| news["kind"] == "said");
        said.map(|news| news["text"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        told(&to_far_end),
        format!("Good morning.{}", ANSWER.concat())
    );
    assert_eq!(
        (told(&to_application), told(&to_plain)),
        (String::new(), String::new())
    );
    // Whoever did not ask is told of no tool.
    let kinds: Vec<_> = facts(&to_plain).into_iter().map(|(_, kind)| kind).collect();
    assert!(
        !kinds.iter().any(|kind| kind.starts_with("tool_")),
        "{kinds:?}"
    );
    assert!(kinds.contains(&"turn_committed".to_owned()));
    assert_eq!(llm.requests().len(), 2);
}

#[test]
fn a_daemon_is_not_started_where_anyone_could_ask_it_everything() {
    let agent = "[agents.desk]\n";
    // Beyond this machine, and no key.
    let (ended, said) = refused_to_start(&format!("listen = \"0.0.0.0:0\"\n{agent}"), &[]);
    assert_eq!(ended.code(), Some(1));
    assert!(
        said.contains("is not this machine alone, and has no control key"),
        "{said}"
    );
    assert_eq!(said.lines().count(), 1, "{said}");

    // A key that is named and is not there, or is none.
    let keyed = format!("[control]\nkey_env = [\"WAVEPORTD_KEY\"]\n{agent}");
    let (ended, said) = refused_to_start(&keyed, &[]);
    assert_eq!(ended.code(), Some(1));
    assert!(
        said.contains("WAVEPORTD_KEY holds no control key"),
        "{said}"
    );
    let (ended, said) = refused_to_start(&keyed, &[("WAVEPORTD_KEY", "typed-by-hand")]);
    assert_eq!(ended.code(), Some(1));
    assert!(said.contains("shorter than 24 characters"), "{said}");
    assert!(!said.contains("typed-by-hand"), "{said}");

    // A file that says what cannot be, and whose words are not said
    // again: somebody may have written a key into it.
    let (ended, said) = refused_to_start("listen = \"127.0.0.1:0\"\n", &[]);
    assert_eq!(ended.code(), Some(1));
    assert!(said.contains("has no agent"), "{said}");
    let written = format!("[control]\nkey = \"made-up-control-key-of-a-test\"\n{agent}");
    let (ended, said) = refused_to_start(&written, &[]);
    assert_eq!(ended.code(), Some(1));
    assert!(
        said.contains("the configuration cannot be read, at line 2"),
        "{said}"
    );
    assert!(!said.contains("made-up"), "{said}");
}
