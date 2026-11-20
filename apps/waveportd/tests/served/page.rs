//! The page a daemon serves to try its agents on.
//!
//! What a browser does with the page is in the tests of the JavaScript
//! client (`sdks/js/test/browser/`). Here is what the daemon answers
//! when it is asked for it.

use std::path::Path;

use serde_json::json;
use waveport_testkit::servers::HttpServer;

use crate::far_end::ask;
use crate::{Served, config, desk};

/// A file of the JavaScript client, as the repository has it.
fn in_the_repository(file: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../sdks/js")
        .join(file);
    std::fs::read_to_string(path).unwrap()
}

#[tokio::test]
async fn a_daemon_on_this_machine_serves_a_page_to_try_its_agents_on() {
    let llm = HttpServer::start(Vec::new()).await;
    let daemon = Served::start(&desk(), &config(&llm, ""), &[]).await;

    // What the page names is beside it, so it is at an address that ends
    // with a stroke, and whoever asks without one is sent there.
    let bare = daemon.ask("GET", "/playground", None).await;
    assert_eq!(
        (bare.status, bare.header("location")),
        (308, Some("/playground/"))
    );
    let page = daemon.ask("GET", "/playground/", None).await;
    assert_eq!(page.status, 200, "{page:?}");
    assert_eq!(
        page.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(page.body, in_the_repository("playground/index.html"));
    // A page from here runs and shows what is from here and nothing
    // else, and is shown in no other page.
    let policy = page.header("content-security-policy").unwrap();
    for rule in [
        "default-src 'none'",
        "script-src 'self'",
        "img-src 'self'",
        "connect-src 'self'",
        "frame-ancestors 'none'",
    ] {
        assert!(policy.contains(rule), "{rule}: {policy}");
    }
    assert!(!policy.contains("unsafe"), "{policy}");
    assert_eq!(page.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(page.header("x-frame-options"), Some("DENY"));
    assert_eq!(page.header("referrer-policy"), Some("no-referrer"));
    // The page is the program's, and the next program may have another:
    // a browser asks whether what it kept is still the page.
    assert_eq!(page.header("cache-control"), Some("no-cache"));
    let allows = page.header("permissions-policy").unwrap();
    assert!(allows.contains("microphone=(self)"), "{allows}");

    // The client is served beside the page, as the repository has it.
    for (name, file, kind) in [
        (
            "playground.js",
            "playground/playground.js",
            "text/javascript",
        ),
        ("playground.css", "playground/playground.css", "text/css"),
        ("icon.svg", "playground/icon.svg", "image/svg+xml"),
        ("sdk/index.js", "src/index.js", "text/javascript"),
        ("sdk/webrtc.js", "src/webrtc.js", "text/javascript"),
        ("sdk/client.js", "src/client.js", "text/javascript"),
        ("sdk/protocol.js", "src/protocol.js", "text/javascript"),
        ("sdk/audio.js", "src/audio.js", "text/javascript"),
        ("sdk/worklet.js", "src/worklet.js", "text/javascript"),
    ] {
        let served = daemon
            .ask("GET", &format!("/playground/{name}"), None)
            .await;
        assert_eq!(served.status, 200, "{name}");
        let said = format!("{kind}; charset=utf-8");
        assert_eq!(served.header("content-type"), Some(said.as_str()), "{name}");
        assert_eq!(served.header("x-content-type-options"), Some("nosniff"));
        assert_eq!(served.body, in_the_repository(file), "{name}");
    }

    // The page is told what there is to try, and that nobody is asked
    // for a key here.
    let told = daemon.ask("GET", "/playground/config.json", None).await;
    assert_eq!(told.header("content-type"), Some("application/json"));
    assert_eq!(told.json(), json!({"agents": ["desk"], "key": false}));

    // Nothing else is there: no folder, and no file of the daemon's.
    for none in [
        "/playground/sdk/",
        "/playground/sdk/nothing.js",
        "/playground/waveportd.toml",
        "/playground/../waveportd.toml",
        "/playground/%2e%2e/waveportd.toml",
        "/playground/index.html",
    ] {
        let answer = daemon.ask("GET", none, None).await;
        assert_eq!(answer.status, 404, "{none}: {answer:?}");
    }
    // And the page is only read.
    let written = daemon.ask("POST", "/playground/", Some("{}")).await;
    assert_eq!(written.status, 405, "{written:?}");

    // It is behind the same look at where a request goes and where it
    // comes from as everything else: a page from elsewhere does not
    // bring it in, and a name that was pointed here is not its name.
    let elsewhere = [("Origin", "https://evil.example")];
    for path in ["/playground/", "/playground/sdk/index.js"] {
        let answer = ask(&daemon.address, "GET", path, &elsewhere, None).await;
        assert_eq!((answer.status, answer.code().as_str()), (403, "forbidden"));
    }
    let renamed = [("Host", "evil.example")];
    let answer = ask(&daemon.address, "GET", "/playground/", &renamed, None).await;
    assert_eq!((answer.status, answer.code().as_str()), (403, "forbidden"));
}

#[tokio::test]
async fn a_daemon_that_is_told_not_to_serves_no_page() {
    let llm = HttpServer::start(Vec::new()).await;
    let daemon = Served::start(&desk(), &config(&llm, "playground = false"), &[]).await;
    for path in [
        "/playground",
        "/playground/",
        "/playground/config.json",
        "/playground/sdk/index.js",
    ] {
        let answer = daemon.ask("GET", path, None).await;
        assert_eq!(answer.status, 404, "{path}: {answer:?}");
    }
    // The rest of it is there as ever.
    let made = daemon
        .ask("POST", "/v1/sessions", Some(r#"{"agent":"desk"}"#))
        .await;
    assert_eq!(made.status, 201, "{made:?}");
}

#[tokio::test]
async fn the_page_of_a_daemon_with_a_key_is_told_to_ask_for_it() {
    let llm = HttpServer::start(Vec::new()).await;
    let keyed = "[control]\nkey_env = [\"WAVEPORTD_KEY\"]\n";
    let key = [("WAVEPORTD_KEY", "a-made-up-key-that-is-long-enough")];
    let daemon = Served::start(&desk(), &config(&llm, keyed), &key).await;
    // The page holds nothing that is anyone's, and is read without the
    // key. What it does with the daemon, it does with the key its user
    // gives it.
    let page = daemon.ask("GET", "/playground/", None).await;
    assert_eq!(page.status, 200, "{page:?}");
    let told = daemon.ask("GET", "/playground/config.json", None).await;
    assert_eq!(told.json(), json!({"agents": ["desk"], "key": true}));
    let made = daemon
        .ask("POST", "/v1/sessions", Some(r#"{"agent":"desk"}"#))
        .await;
    assert_eq!(made.status, 401, "{made:?}");
}
