//! A daemon that stops, and the one that is started after it.
//!
//! A daemon stops because it is told to, or because its machine does:
//! the first is asked of it as whoever runs it would, and the second is
//! a kill at a place the test names (`WAVEPORT_TEST_HALT`, as
//! `waveport-assembly` has it). Either way its sessions are in the file
//! they are kept in, and the next daemon goes on with one when its far
//! end comes back.

use core::time::Duration;

use serde_json::json;
use waveport_protocol::client::v1::View;
use waveport_testkit::servers::HttpServer;

use crate::far_end::Event;
use crate::{ANSWER, Served, answer, config, desk, is_fact, slow_answer, viewed};

/// Where the sessions of these daemons are kept.
const KEPT: &str = "[journal]\nfile = \"sessions.redb\"\n";

/// Whether the event is the fact that ends a response whose text is
/// `text`.
fn says(event: &Event, text: &str) -> bool {
    is_fact(event, "utterance_committed") && event.json()["text"] == text
}

/// What the conversation of a view holds: who said what, in order.
fn conversation(view: &View) -> Vec<String> {
    let state = serde_json::to_value(view.state()).unwrap();
    state["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| format!("{}: {}", item["kind"].as_str().unwrap(), item["text"]))
        .collect()
}

#[cfg(unix)]
#[tokio::test]
async fn a_daemon_that_is_told_to_stop_lets_an_answer_end_and_says_that_it_goes() {
    // An answer that takes a while to be written: long enough for a
    // machine that is slow today to ask the daemon three things before
    // it has ended.
    let slowly = slow_answer(&ANSWER, Duration::from_millis(250));
    let llm = HttpServer::start(vec![slowly, answer(&["Half past", " three."], None)]).await;
    let desk = desk();
    let mut daemon = Served::start(&desk, &config(&llm, KEPT), &[]).await;
    let (name, token) = daemon.session().await;
    let mut events = daemon
        .follow(&name, "?features=said", Some(&token), &[])
        .await
        .unwrap();
    let welcome = events.next().await.unwrap().json();
    let resume = welcome["resume_token"].as_str().unwrap().to_owned();
    let mut told = events.until(|event| says(event, "Good morning.")).await;
    daemon.types(&name, &resume, "What time is it?").await;
    // The answer is under way when the daemon is told to stop.
    let begun = |event: &Event| event.kind == "told" && event.json()["kind"] == "said";
    told.extend(events.until(begun).await);
    daemon.tell_to_stop();

    // From then on it takes nothing new, and says so to whoever asks.
    let mut health = daemon.ask("GET", "/healthz", None).await;
    for _ in 0..200 {
        if health.status == 503 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        health = daemon.ask("GET", "/healthz", None).await;
    }
    assert_eq!(
        (health.status, health.json()),
        (503, json!({"status": "stopping"}))
    );
    let make = Some(r#"{"agent":"desk"}"#);
    let refused = daemon.ask("POST", "/v1/sessions", make).await;
    assert_eq!((refused.status, refused.code().as_str()), (503, "busy"));
    assert_eq!(refused.header("retry-after"), Some("1"));
    // Nor is a client given a line: one that comes back is to try the
    // daemon that is started next.
    let line = crate::lined::asks_for_a_line(&daemon, "/v1/client", &[]).await;
    assert_eq!((line.status, line.code().as_str()), (503, "busy"));

    // The answer comes to its end. Then whoever follows is told that
    // the daemon goes, and the stream ends.
    let rest = events.rest().await;
    let whole = ANSWER.concat();
    let answered = rest.iter().find(|event| says(event, &whole));
    let answered = answered.unwrap_or_else(|| panic!("{rest:#?}"));
    assert_eq!(answered.json()["truncated"], false);
    let last = rest.last().unwrap();
    assert_eq!(last.json(), json!({"type": "bye", "reason": "going_away"}));
    let byes = rest.iter().filter(|event| event.kind == "bye").count();
    assert_eq!(byes, 1, "{rest:#?}");
    told.extend(rest);
    let before = viewed(View::new(), &told);
    // An answer that takes long has facts of its own on the way: how
    // far it had got, each second.
    let position = format!("1.{}", before.seen());
    assert!(before.seen() >= 4, "{position}");
    let ended = daemon.ended().await;
    assert!(ended.success(), "{ended:?}: {}", daemon.errors());
    let errors = daemon.errors();
    assert!(
        errors.contains("sessions that were stopped: 1. what they had come to is kept"),
        "{errors}"
    );
    assert!(errors.ends_with("waveportd: stopped"), "{errors}");
    assert!(!errors.contains("still under way"), "{errors}");
    drop(daemon);

    // The daemon that is started next has the session, which does not
    // run until its far end comes back.
    let daemon = Served::start(&desk, &config(&llm, KEPT), &[]).await;
    daemon.noted("1 session is kept, and is gone on with").await;
    let broken = json!({"session": name, "agent": "desk", "state": "broken"});
    let listed = daemon.ask("GET", "/v1/sessions?state=broken", None).await;
    assert_eq!(listed.json(), json!({"sessions": [broken]}));
    let running = daemon.ask("GET", "/v1/sessions?state=running", None).await;
    assert_eq!(running.json(), json!({"sessions": []}));

    // It comes back with what it was sent last. That was told by a life
    // of the session that is over, so it is told what the session has
    // come to: the same as what it held.
    let last = [("Last-Event-ID", position.as_str())];
    let mut back = daemon
        .follow(&name, "?features=said", Some(&resume), &last)
        .await
        .unwrap();
    let caught_up = back.until(|event| event.kind == "state").await;
    let welcomed = caught_up[0].json();
    assert_eq!(
        (&welcomed["life"], &welcomed["resumed"]),
        (&json!(2), &json!(true))
    );
    assert_eq!(welcomed.get("resume_token"), None);
    assert_eq!(caught_up[1].kind, "gap");
    let after = viewed(before.clone(), &caught_up);
    assert_eq!(conversation(&after), conversation(&before));
    assert_eq!(conversation(&after).len(), 3, "{:?}", conversation(&after));
    // And the session goes on: with what was said before the daemon
    // stopped.
    daemon.types(&name, &resume, "And now?").await;
    let more = back.until(|event| says(event, "Half past three.")).await;
    let position = more.last().unwrap().id.clone().unwrap();
    assert!(position.starts_with("2."), "{position}");
    let asked = llm.requests();
    assert_eq!(asked.len(), 2);
    assert!(asked[1].body.contains(&whole) && asked[1].body.contains("And now?"));
    let info = daemon
        .ask("GET", &format!("/v1/sessions/{name}"), None)
        .await;
    assert_eq!(info.json()["state"], "running");
    assert_eq!(info.json()["life"], 2);
}

#[cfg(unix)]
#[tokio::test]
async fn the_turns_of_a_session_are_traced_under_its_name() {
    use waveport_testkit::servers::HttpAnswer;

    let llm = HttpServer::start(vec![answer(&ANSWER, None)]).await;
    // A server that stands in for a collector, and takes what it is
    // sent.
    let accepts = HttpAnswer::Status {
        status: 200,
        headers: Vec::new(),
        body: String::new(),
    };
    let collector = HttpServer::start(vec![accepts]).await;
    let telemetry = format!("[telemetry]\notlp = \"{}\"\n", collector.url(""));
    let mut daemon = Served::start(&desk(), &config(&llm, &telemetry), &[]).await;
    let (name, token) = daemon.session().await;
    let mut events = daemon.follow(&name, "", Some(&token), &[]).await.unwrap();
    let welcome = events.next().await.unwrap().json();
    let resume = welcome["resume_token"].as_str().unwrap().to_owned();
    daemon.types(&name, &resume, "What time is it?").await;
    let whole = ANSWER.concat();
    events.until(|event| says(event, &whole)).await;
    // What is still to be sent is sent as the daemon stops.
    daemon.tell_to_stop();
    assert!(daemon.ended().await.success(), "{}", daemon.errors());
    let errors = daemon.errors();
    assert!(
        errors.contains("traces and metrics of the calls go to"),
        "{errors}"
    );
    assert!(!errors.contains("did not reach"), "{errors}");

    let sent = collector.requests();
    let traces: String = sent
        .iter()
        .filter(|request| request.line == "POST /v1/traces HTTP/1.1")
        .map(|request| request.body.as_str())
        .collect();
    // The turn and its response are there, under the name the API
    // gives the session: that is what joins the turns of one.
    for held in ["turn", "response", "gen_ai.conversation.id", name.as_str()] {
        assert!(traces.contains(held), "no {held} among the traces");
    }
    // No word of the session left the machine that way, and no token.
    for kept_back in [
        "What time is it?",
        whole.as_str(),
        "Good morning.",
        "wpr1_",
        "wpc1_",
    ] {
        assert!(!traces.contains(kept_back), "{kept_back} among the traces");
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one session from its beginning to its end, told in the order it happens"
)]
async fn a_session_goes_on_in_the_daemon_that_is_started_after_one_was_killed() {
    // The first answer takes a while to be written. The journal takes
    // what waits for it together: an answer that is written at once
    // could be stored with the question it answers.
    let script = vec![
        slow_answer(&ANSWER, Duration::from_millis(25)),
        answer(&["Half past", " three."], None),
        answer(&["You are", " welcome."], None),
    ];
    let llm = HttpServer::start(script).await;
    let desk = desk();
    // The daemon's journal stops once it has stored what was typed, and
    // everything else goes on: the answer is written and told, and is
    // stored nowhere when the daemon is killed.
    let halt = [("WAVEPORT_TEST_HALT", "after:turn_committed")];
    let mut daemon = Served::start(&desk, &config(&llm, KEPT), &halt).await;
    let (name, token) = daemon.session().await;
    let mut events = daemon
        .follow(&name, "?features=said", Some(&token), &[])
        .await
        .unwrap();
    let welcome = events.next().await.unwrap().json();
    let resume = welcome["resume_token"].as_str().unwrap().to_owned();
    daemon.types(&name, &resume, "What time is it?").await;
    let first = ANSWER.concat();
    let told = events.until(|event| says(event, &first)).await;
    assert_eq!(told.last().unwrap().id.as_deref(), Some("1.4"));
    let before = viewed(View::new(), &told);
    daemon.halted_at("after:turn_committed").await;
    daemon.kill();
    // The stream was cut: nobody said that it was over.
    let rest = events.rest().await;
    assert!(rest.iter().all(|event| event.kind != "bye"), "{rest:#?}");
    drop(daemon);

    // The next daemon has the session, as one that broke off.
    let daemon = Served::start(&desk, &config(&llm, KEPT), &[]).await;
    let broken = json!({"session": name, "agent": "desk", "state": "broken"});
    let listed = daemon.ask("GET", "/v1/sessions?state=broken", None).await;
    assert_eq!(listed.json(), json!({"sessions": [broken]}));
    let at = format!("/v1/sessions/{name}");
    assert_eq!(daemon.ask("GET", &at, None).await.json(), broken);
    // A token of a daemon that is gone is good with the next: it is
    // kept with the session. One that was made up is not.
    let forged = format!("wpr1_{name}{}", "0".repeat(32));
    let none = daemon
        .follow(&name, "", Some(&forged), &[])
        .await
        .err()
        .unwrap();
    assert_eq!((none.status, none.code().as_str()), (401, "token"));

    // An application looks at the session before its far end is back,
    // which brings it back. It asked for all of it, and is sent the
    // facts the journal had, and then what this life of the session
    // does: it says that something was cut off, and gives the answer it
    // owes. The fact numbered 4 is now another than the one the far end
    // was shown, and the answer is another answer.
    let second = "Half past three.";
    let mut application = daemon.follow(&name, "", None, &[]).await.unwrap();
    let seen = application.until(|event| says(event, second)).await;
    let welcomed = seen[0].json();
    assert_eq!(
        (&welcomed["life"], &welcomed["resumed"]),
        (&json!(2), &json!(false))
    );
    let ids: Vec<_> = seen
        .iter()
        .filter_map(|event| event.id.as_deref())
        .collect();
    assert_eq!(ids[..4], ["2.1", "2.2", "2.3", "2.4"], "{seen:#?}");
    let fourth = seen.iter().find(|event| event.id.as_deref() == Some("2.4"));
    assert_eq!(
        fourth.unwrap().json(),
        json!({"type": "fact", "seq": 4, "kind": "cut_off", "cause": "recovery"})
    );
    let made = seen.last().unwrap().json()["seq"].as_u64().unwrap();
    assert!(made > 4, "{seen:#?}");
    let whole = [
        "utterance: \"Good morning.\"",
        "turn: \"What time is it?\"",
        "utterance: \"Half past three.\"",
    ];
    assert_eq!(conversation(&viewed(View::new(), &seen)), whole);
    let running = daemon.ask("GET", &at, None).await.json();
    assert_eq!(
        (&running["state"], &running["life"], &running["next_seq"]),
        (&json!("running"), &json!(2), &json!(made + 1))
    );
    let listed = daemon.ask("GET", "/v1/sessions?state=broken", None).await;
    assert_eq!(listed.json(), json!({"sessions": []}));

    // The far end comes back and says that it was sent the fact
    // numbered 4. The session has a fact of that number and more after
    // it, and no number shows that the far end's was another one: only
    // that it was told by a life of the session that is over. Sent the
    // facts after 4, it would hold an answer that was never given and
    // one that was. So it is told what the session has come to, in
    // place of what it holds.
    let last = [("Last-Event-ID", "1.4")];
    let mut back = daemon
        .follow(&name, "?features=said", Some(&resume), &last)
        .await
        .unwrap();
    let caught_up = back.until(|event| event.kind == "state").await;
    let kinds: Vec<_> = caught_up.iter().map(|event| event.kind.as_str()).collect();
    assert_eq!(kinds, ["welcome", "gap", "state"]);
    let welcomed = caught_up[0].json();
    assert_eq!(
        (&welcomed["life"], &welcomed["resumed"]),
        (&json!(2), &json!(true))
    );
    assert_eq!(
        caught_up[1].json(),
        json!({"type": "gap", "resumes": made + 1})
    );
    let position = format!("2.{made}");
    assert_eq!(caught_up[2].id.as_deref(), Some(position.as_str()));
    let mut after = viewed(before.clone(), &caught_up);
    assert_eq!(conversation(&after), whole);
    // What the client held before was of a life that is over.
    assert!(
        conversation(&before)
            .iter()
            .any(|item| item.contains(&first))
    );
    assert_eq!(back.idles_for(Duration::from_millis(200)).await, 0);

    // The session goes on from there.
    daemon.types(&name, &resume, "Thank you.").await;
    let thanked = back.until(|event| says(event, "You are welcome.")).await;
    after = viewed(after, &thanked);
    assert_eq!(conversation(&after).len(), 5);
    let asked = llm.requests();
    assert_eq!(asked.len(), 3);
    // The model was told what the session holds: the answer nobody
    // stored is not part of it.
    assert!(asked[2].body.contains(second) && !asked[2].body.contains(&first));
}
