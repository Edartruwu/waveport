//! No recording holds a key.
//!
//! The tests of the provider adapters replay what the providers said, as
//! it was recorded with a real key in hand. A provider repeats a key it
//! does not know, and a tool that records a session can write a header
//! down with it. The test here reads every file under a directory named
//! `fixtures` and fails on anything that has the shape of a key.

use std::path::{Path, PathBuf};

/// What a key begins with, or follows, and whether that has to stand at
/// the beginning of a word.
const BEFORE_A_KEY: [(&str, bool); 5] = [
    ("sk-", true),
    ("sk_", true),
    ("Bearer ", false),
    ("Token ", false),
    ("key=", false),
];

/// How many characters of a key's alphabet in a row make a key. The made
/// up keys of the tests are shorter, and every real one is longer.
const LONG: usize = 20;

/// The first thing in `text` that has the shape of a key.
fn key_in(text: &str) -> Option<&str> {
    for (prefix, whole_word) in BEFORE_A_KEY {
        for (at, _) in text.match_indices(prefix) {
            let before = text.get(..at).and_then(|head| head.chars().next_back());
            if whole_word && before.is_some_and(char::is_alphanumeric) {
                continue;
            }
            let rest = text.get(at + prefix.len()..).unwrap_or_default();
            let run = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .count();
            if run >= LONG {
                return text.get(at..at + prefix.len() + run);
            }
        }
    }
    None
}

/// Every file under `dir`, and under the directories in it.
fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Every directory named `fixtures` under `dir`. What a build leaves
/// behind is not looked into.
fn fixtures_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if !path.is_dir() {
            continue;
        }
        match path.file_name().and_then(|name| name.to_str()) {
            Some("target" | ".git") => {}
            Some("fixtures") => out.push(path),
            _ => fixtures_under(&path, out),
        }
    }
}

#[test]
fn a_key_is_seen_and_what_only_looks_like_one_is_not() {
    for text in [
        "Incorrect API key provided: sk-proj-Zx81kQ0tTq7Lm3cYw2Vb9.",
        "{\"key\":\"sk_car_9fQ2mB7xLr4TzK1vN8pWd3\"}",
        "authorization: Bearer 9fQ2mB7xLr4TzK1vN8pWd3aa",
        "Authorization: Token 0123456789abcdef0123456789abcdef01234567",
        "wss://example.test/listen?api_key=0123456789abcdef0123",
    ] {
        assert!(key_in(text).is_some(), "{text}");
    }
    assert_eq!(
        key_in("was given sk-proj-Zx81kQ0tTq7Lm3cYw2Vb9, and"),
        Some("sk-proj-Zx81kQ0tTq7Lm3cYw2Vb9")
    );
    for text in [
        // The key the tests make up.
        "Incorrect API key provided: sk-not-a-key.",
        // The names a provider gives its sessions and events.
        "\"event_id\":\"event_EkbFE8vAVBes8YeVYB7F6\",\"id\":\"sess_EkaJAKAcnJghZq4g6QfJw\"",
        // A word that only ends as a key begins.
        "a_task_needs_nothing_said_about_it_at_all",
        "Bearer of bad news",
    ] {
        assert_eq!(key_in(text), None, "{text}");
    }
}

#[test]
fn no_recording_holds_a_key() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut fixtures = Vec::new();
    for part in ["apps", "crates"] {
        fixtures_under(&root.join(part), &mut fixtures);
    }
    let mut files = Vec::new();
    for dir in &fixtures {
        files_under(dir, &mut files);
    }
    // A test that reads nothing finds nothing.
    assert!(files.len() >= 10, "only {} recordings found", files.len());
    for file in files {
        let bytes = std::fs::read(&file).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        if let Some(key) = key_in(&text) {
            let shown: String = key.chars().take(8).collect();
            panic!(
                "{} holds what looks like a key: it begins {shown:?}",
                file.display()
            );
        }
    }
}
