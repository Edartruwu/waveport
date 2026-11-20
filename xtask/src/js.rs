//! The JavaScript client (`sdks/js`): the commands that hold it, and what
//! its packages may depend on.
//!
//! The client runs in a browser as it is written, with nothing bundled
//! and nothing it depends on, so that the daemon can serve it as part
//! of itself. What is installed is for checking it: the TypeScript
//! compiler, which reads the types its comments give, and the types of
//! Node, for the tests. A test here fails when a package takes on
//! anything else, or takes it some other way than pinned, locked and a
//! week old.
//!
//! One package is no client of ours (`sdks/pipecat-stock`): the stock
//! client of Pipecat's on a page, which the daemon's gateway is held
//! against in a browser. It is a test and ships nothing. What it is
//! installed with is that client, its transport and what bundles them
//! for a page, and what those bring is listed here by name: a name
//! that is not listed is something the client took on since, and is
//! looked at before it is installed.

/// Where the client is, from the root of the repository.
pub(crate) const DIR: &str = "sdks/js";

/// Installs what the lock file names, and nothing else.
pub(crate) const INSTALL: &str = "npm ci";
/// Checks the types the comments give.
pub(crate) const CHECK: &str = "npm run check";
/// The tests that need no browser.
pub(crate) const TEST: &str = "npm test";
/// The daemon with stand-ins for its providers, for the browser.
pub(crate) const STAND_IN: &str = "cargo build --locked -p waveportd --example stand_in";
/// A browser on the page that daemon serves.
pub(crate) const BROWSER: &str =
    "node test/browser/run.js --daemon ../../target/debug/examples/stand_in";

/// Where the stock client of Pipecat's is held against the daemon.
pub(crate) const STOCK_DIR: &str = "sdks/pipecat-stock";
/// A browser with that client on a page, against that daemon.
pub(crate) const STOCK: &str = "node test/run.js --daemon ../../target/debug/examples/stand_in";
/// The newest of that client that is a week old, in place of what the
/// lock file names: for the run that says when the client has moved.
/// Only the workflow of every night runs it, which a test holds to it.
#[cfg(test)]
pub(crate) const STOCK_NEWEST: &str = "npm install --no-save --no-package-lock \
     @pipecat-ai/client-js@latest @pipecat-ai/small-webrtc-transport@latest";

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use serde_json::Value;

    /// What a package here may be installed with, and nothing may be
    /// installed for it to run.
    const TOOLS: [&str; 2] = ["typescript", "@types/node"];

    /// What those bring with them: the compiler as it is built for each
    /// kind of machine, and the types of what Node fetches with.
    fn is_brought(name: &str) -> bool {
        TOOLS.contains(&name)
            || name == "undici-types"
            || name.starts_with("@typescript/typescript-")
    }

    /// The package that is a test of the daemon and no client of ours.
    const STOCK: &str = "pipecat-stock";

    /// What it is installed with: the stock client of Pipecat's, its
    /// transport, and what bundles them for a page.
    const STOCK_TOOLS: [&str; 3] = [
        "@pipecat-ai/client-js",
        "@pipecat-ai/small-webrtc-transport",
        "esbuild",
    ];

    /// What those bring with them, by name.
    const STOCK_BRINGS: [&str; 22] = [
        "@babel/runtime",
        "@daily-co/daily-js",
        "@sentry-internal/browser-utils",
        "@sentry-internal/feedback",
        "@sentry-internal/replay",
        "@sentry-internal/replay-canvas",
        "@sentry/browser",
        "@sentry/core",
        "@types/events",
        "bowser",
        "clone-deep",
        "dequal",
        "events",
        "is-plain-object",
        "isobject",
        "kind-of",
        "lodash",
        "rxjs",
        "shallow-clone",
        "tslib",
        "typed-emitter",
        "uuid",
    ];

    /// Those, and the bundler as it is built for each kind of machine.
    fn is_brought_by_the_stock_client(name: &str) -> bool {
        STOCK_TOOLS.contains(&name) || STOCK_BRINGS.contains(&name) || name.starts_with("@esbuild/")
    }

    /// Every `package.json` under `dir` that is not something installed.
    fn packages_under(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().and_then(|name| name.to_str());
            if path.is_dir() {
                if name != Some("node_modules") {
                    packages_under(&path, out);
                }
            } else if name == Some("package.json") {
                out.push(path);
            }
        }
    }

    fn json(path: &Path) -> Value {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    }

    /// Whether a version is one version, and not a range of them or a
    /// place to fetch from.
    fn is_exact(version: &str) -> bool {
        let mut parts = version.split('.');
        let numbers = parts
            .by_ref()
            .take(3)
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
        numbers && version.split('.').count() == 3 && parts.next().is_none()
    }

    #[test]
    fn an_exact_version_is_told_from_a_range() {
        for exact in ["7.0.2", "24.19.2", "0.0.101"] {
            assert!(is_exact(exact), "{exact}");
        }
        for range in [
            "^7.0.2",
            "~7.0.2",
            "7.0",
            "7",
            "7.x",
            "*",
            "latest",
            ">=7.0.2",
            "7.0.2 || 8.0.0",
            "7.0.2-beta",
            "",
            "npm:other@7.0.2",
            "https://example.test/a.tgz",
            "7.0.2.1",
        ] {
            assert!(!is_exact(range), "{range}");
        }
    }

    #[test]
    fn packages_run_on_nothing_and_are_checked_with_what_is_listed() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../sdks");
        let mut packages = Vec::new();
        packages_under(&root, &mut packages);
        assert!(!packages.is_empty(), "no package was found under sdks");
        for path in packages {
            let at = path.display();
            let package = json(&path);
            assert_eq!(package["license"], "Apache-2.0", "{at}");
            // Nothing is needed to run it.
            for kind in [
                "dependencies",
                "peerDependencies",
                "optionalDependencies",
                "bundleDependencies",
                "bundledDependencies",
            ] {
                let none = package
                    .get(kind)
                    .is_none_or(|held| held.as_object().is_some_and(serde_json::Map::is_empty));
                assert!(none, "{at} has {kind}");
            }
            // What it is checked with is listed here, and is one version.
            let folder = path.parent().unwrap();
            let stock = folder.file_name().is_some_and(|name| name == STOCK);
            let (listed, brought): (&[&str], fn(&str) -> bool) = if stock {
                (&STOCK_TOOLS, is_brought_by_the_stock_client)
            } else {
                (&TOOLS, is_brought)
            };
            let tools = package["devDependencies"].as_object();
            for (name, version) in tools.into_iter().flatten() {
                assert!(
                    listed.contains(&name.as_str()),
                    "{at} is checked with {name}"
                );
                let version = version.as_str().unwrap_or_default();
                assert!(is_exact(version), "{at} takes {name} at {version:?}");
            }
            // Nothing runs as it is installed, and nothing is installed
            // that is not a week old.
            let rules = std::fs::read_to_string(folder.join(".npmrc"))
                .unwrap_or_else(|error| panic!("{at} has no .npmrc beside it: {error}"));
            for rule in ["min-release-age=7", "ignore-scripts=true"] {
                assert!(
                    rules.lines().any(|line| line.trim() == rule),
                    "{at}: {rule}"
                );
            }
            // What is installed is locked, each with what it has to
            // hash to, from the one registry.
            let lock = json(&folder.join("package-lock.json"));
            assert_eq!(lock["lockfileVersion"], 3, "{at}");
            let locked = lock["packages"].as_object().unwrap();
            assert!(
                locked.len() > 1 || tools.is_none_or(serde_json::Map::is_empty),
                "{at}"
            );
            for (place, held) in locked.iter().filter(|(place, _)| !place.is_empty()) {
                let name = place.rsplit("node_modules/").next().unwrap_or_default();
                assert!(brought(name), "{at} locks {name}");
                assert!(held["dev"] == true, "{at} locks {name} to run with");
                let from = held["resolved"].as_str().unwrap_or_default();
                assert!(
                    from.starts_with("https://registry.npmjs.org/"),
                    "{at}: {name}"
                );
                let hash = held["integrity"].as_str().unwrap_or_default();
                assert!(hash.starts_with("sha512-"), "{at}: {name} has no hash");
            }
        }
    }
}
