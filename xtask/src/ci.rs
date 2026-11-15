//! `cargo xtask ci`: runs every gate locally, cheapest first.
//!
//! The command strings below are the gates. CI runs the same strings; a test
//! in this module fails if the workflows and this file drift apart.

use std::process::{Command, Stdio};
use std::time::Instant;

use crate::args::Args;
use crate::error::{Context, Error, Result};
use crate::meta::{self, LoadOptions, Workspace};
use crate::{arch, assets, bench, git, lint_sync, semver, tier};

/// The dated nightly that runs Miri. Change it here and in the workflow together.
macro_rules! miri_toolchain {
    () => {
        "nightly-2026-11-14"
    };
}

const FMT: &str = "cargo fmt --all --check";
const CLIPPY: &str = "cargo clippy --workspace --all-targets --all-features --locked";
const TEST: &str = "cargo nextest run --workspace --all-features --locked --profile ci";
const DOCTEST: &str = "cargo test --workspace --doc --all-features --locked";
const DOCS: &str = "cargo doc --workspace --no-deps --all-features --locked";
const NO_STD: &str = "cargo check -p waveport-domain --target x86_64-unknown-none --locked";
const FEATURES: &str = "cargo hack check --feature-powerset --workspace --locked";
const MSRV: &str = "cargo hack check --rust-version --workspace --all-targets --locked";
const DENY: &str = "cargo deny check";
const BENCH_SMOKE: &str = "cargo bench -p waveport-benches --bench wall_smoke --locked -- --test";
/// Tries every way the threads of the compute plane can take turns over
/// what they hand each other. The build is one of its own, made with
/// [`LOOM_FLAGS`] and kept apart so that it and the ordinary one do not
/// throw each other's work away.
const LOOM: &str =
    "cargo test --release --locked -p waveport-runtime --lib --target-dir target/loom";
const LOOM_FLAGS: &str = "--cfg waveport_loom";
const MIRI: &str = concat!(
    "rustup run ",
    miri_toolchain!(),
    " cargo miri test -p waveport-alloc-counter"
);

/// A tool a gate needs that may not be installed.
#[derive(Clone, Copy)]
enum Tool {
    Nextest,
    Hack,
    Deny,
    SemverChecks,
    Miri,
    Valgrind,
}

impl Tool {
    fn installed(self) -> bool {
        let probe = |command: &str| {
            let mut parts = command.split_whitespace();
            parts.next().is_some_and(|program| {
                Command::new(program)
                    .args(parts)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success())
            })
        };
        match self {
            Self::Nextest => probe("cargo nextest --version"),
            Self::Hack => probe("cargo hack --version"),
            Self::Deny => probe("cargo deny --version"),
            Self::SemverChecks => probe("cargo semver-checks --version"),
            Self::Miri => probe(concat!(
                "rustup run ",
                miri_toolchain!(),
                " cargo miri --version"
            )),
            Self::Valgrind => bench::tools_installed(),
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Nextest => "cargo install --locked cargo-nextest",
            Self::Hack => "cargo install --locked cargo-hack",
            Self::Deny => "cargo install --locked cargo-deny",
            Self::SemverChecks => "cargo install --locked cargo-semver-checks",
            Self::Miri => concat!(
                "rustup toolchain install ",
                miri_toolchain!(),
                " --profile minimal --component miri --component rust-src"
            ),
            Self::Valgrind => "install valgrind, then cargo install --locked gungraun-runner",
        }
    }
}

/// What a gate needs to run.
struct Env {
    workspace: Workspace,
    /// The commit shared with `main`, used as the base for comparisons.
    base: Option<String>,
}

/// One gate: a name, whether `--fast` includes it, a tool it needs, and how to run it.
struct Gate {
    name: &'static str,
    fast: bool,
    tool: Option<Tool>,
    run: fn(&Env) -> Result<bool>,
}

const GATES: &[Gate] = &[
    Gate {
        name: "fmt",
        fast: true,
        tool: None,
        run: |env| shell(env, FMT),
    },
    Gate {
        name: "policy",
        fast: true,
        tool: None,
        run: |_| Ok(arch::run(&[])? & tier::run(&[])? & lint_sync::run(&[])?),
    },
    Gate {
        name: "clippy",
        fast: true,
        tool: None,
        run: |env| shell(env, CLIPPY),
    },
    Gate {
        name: "test",
        fast: true,
        tool: Some(Tool::Nextest),
        // Some tests run the models that listen, which are files the
        // repository does not carry. They are fetched first, which takes
        // no time once they are there.
        run: |env| Ok(assets::run(&[])? && shell(env, TEST)?),
    },
    Gate {
        name: "doctest",
        fast: true,
        tool: None,
        run: |env| shell(env, DOCTEST),
    },
    Gate {
        name: "docs",
        fast: false,
        tool: None,
        run: |env| {
            command(env, DOCS, true)?
                .env("RUSTDOCFLAGS", "-D warnings")
                .status()
                .map(|status| status.success())
                .context("could not run `cargo doc`")
        },
    },
    Gate {
        name: "no-std",
        fast: false,
        tool: None,
        run: |env| shell(env, NO_STD),
    },
    Gate {
        name: "features",
        fast: false,
        tool: Some(Tool::Hack),
        run: |env| shell(env, FEATURES),
    },
    Gate {
        name: "msrv",
        fast: false,
        tool: Some(Tool::Hack),
        // Warnings are not denied here: an older compiler may warn about
        // things the pinned one accepts.
        run: |env| {
            command(env, MSRV, false)?
                .status()
                .map(|status| status.success())
                .context("could not run `cargo hack`")
        },
    },
    Gate {
        name: "deny",
        fast: false,
        tool: Some(Tool::Deny),
        run: |env| shell(env, DENY),
    },
    Gate {
        name: "semver",
        fast: false,
        tool: Some(Tool::SemverChecks),
        run: |env| {
            let self_test = semver::run(&["--self-test".to_owned()])?;
            let gate = if let Some(base) = &env.base {
                semver::check(&env.workspace, base, false)?
            } else {
                println!("semver: no common commit with `main`; nothing to compare");
                true
            };
            Ok(self_test & gate)
        },
    },
    Gate {
        name: "bench-smoke",
        fast: false,
        tool: None,
        run: |env| shell(env, BENCH_SMOKE),
    },
    Gate {
        name: "loom",
        fast: false,
        tool: None,
        run: |env| {
            command(env, LOOM, true)?
                .env("RUSTFLAGS", LOOM_FLAGS)
                .status()
                .map(|status| status.success())
                .context("could not run the loom schedules")
        },
    },
    Gate {
        name: "miri",
        fast: false,
        tool: Some(Tool::Miri),
        run: |env| shell(env, MIRI),
    },
    Gate {
        name: "bench-ir",
        fast: false,
        tool: Some(Tool::Valgrind),
        run: |env| bench::compare(&env.workspace, env.base.as_deref()),
    },
];

/// Builds a command from one of the gate strings.
fn command(env: &Env, line: &str, deny_warnings: bool) -> Result<Command> {
    let mut parts = line.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| Error::new("empty gate command"))?;
    let mut command = Command::new(program);
    command.current_dir(&env.workspace.root).args(parts);
    if deny_warnings {
        command.env("CARGO_BUILD_WARNINGS", "deny");
    } else {
        command.env_remove("CARGO_BUILD_WARNINGS");
    }
    println!(
        "    $ {}",
        line.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    Ok(command)
}

/// Runs one of the gate strings with warnings denied.
fn shell(env: &Env, line: &str) -> Result<bool> {
    command(env, line, true)?
        .status()
        .map(|status| status.success())
        .context(format!("could not run `{line}`"))
}

/// Runs the gates.
pub(crate) fn run(args: &[String]) -> Result<bool> {
    let mut args = Args::new(args);
    let fast = args.flag("--fast");
    let keep_going = args.flag("--keep-going");
    let strict = args.flag("--strict");
    let only = args.value("--only")?;
    args.finish()?;
    if let Some(name) = &only
        && !GATES.iter().any(|gate| gate.name == name)
    {
        let names: Vec<&str> = GATES.iter().map(|gate| gate.name).collect();
        return Err(Error::new(format!(
            "unknown gate `{name}`; gates: {}",
            names.join(", ")
        )));
    }

    let workspace = meta::load(&LoadOptions::default())?;
    let base = git::merge_base(&workspace.root, "main");
    let env = Env { workspace, base };
    let selected: Vec<&Gate> = GATES
        .iter()
        .filter(|gate| {
            only.as_deref()
                .map_or(!fast || gate.fast, |name| gate.name == name)
        })
        .collect();

    let mut failed = Vec::new();
    let mut skipped = Vec::new();
    let started = Instant::now();
    for (index, gate) in selected.iter().enumerate() {
        println!("==> [{}/{}] {}", index + 1, selected.len(), gate.name);
        if let Some(tool) = gate.tool
            && !tool.installed()
        {
            println!("    not installed: {}", tool.hint());
            if strict {
                failed.push(gate.name);
                if !keep_going {
                    break;
                }
            } else {
                skipped.push(gate.name);
            }
            continue;
        }
        if !(gate.run)(&env)? {
            failed.push(gate.name);
            if !keep_going {
                break;
            }
        }
    }

    println!();
    if !skipped.is_empty() {
        println!("skipped (tool not installed): {}", skipped.join(", "));
    }
    let seconds = started.elapsed().as_secs();
    if failed.is_empty() {
        let ran = selected.len() - skipped.len();
        println!("ci: {ran} gates passed in {seconds}s");
        Ok(true)
    } else {
        eprintln!("ci: FAILED: {}", failed.join(", "));
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        BENCH_SMOKE, CLIPPY, DENY, DOCS, DOCTEST, FEATURES, FMT, GATES, LOOM, LOOM_FLAGS, MIRI,
        MSRV, NO_STD, TEST, Tool,
    };

    #[test]
    fn gate_names_are_unique() {
        let mut names: Vec<&str> = GATES.iter().map(|gate| gate.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), GATES.len());
    }

    #[test]
    fn fast_gates_run_before_slow_ones() {
        let first_slow = GATES.iter().position(|gate| !gate.fast);
        let last_fast = GATES.iter().rposition(|gate| gate.fast);
        assert!(
            last_fast < first_slow,
            "a fast gate is listed after a slow one"
        );
    }

    /// The text of a workflow, with runs of whitespace collapsed.
    fn workflow(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../.github/workflows")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[track_caller]
    fn assert_runs(workflow_name: &str, text: &str, commands: &[&str]) {
        for command in commands {
            assert!(
                text.contains(command),
                "`.github/workflows/{workflow_name}` does not run `{command}`"
            );
        }
    }

    /// CI and `cargo xtask ci` must run the same gates. A gate changed in
    /// one place and not the other fails here.
    #[test]
    fn ci_workflow_runs_the_same_commands() {
        let text = workflow("ci.yml");
        assert_runs(
            "ci.yml",
            &text,
            &[
                FMT,
                CLIPPY,
                TEST,
                DOCTEST,
                DOCS,
                NO_STD,
                FEATURES,
                MSRV,
                DENY,
                BENCH_SMOKE,
                LOOM,
                MIRI,
                Tool::Miri.hint(),
                // Run in-process by `cargo xtask ci`.
                "cargo xtask arch-check",
                "cargo xtask tier-check",
                "cargo xtask lint-sync",
                "cargo xtask semver --self-test",
                "cargo xtask bench-ir --base",
                "cargo xtask fetch-assets",
                // Settings the local runner applies to its child processes.
                "CARGO_BUILD_WARNINGS: deny",
                "RUSTDOCFLAGS: -D warnings",
                &format!("RUSTFLAGS: {LOOM_FLAGS}"),
            ],
        );
    }

    /// What runs every night and not on pull requests is named in
    /// `CONTRIBUTING.md` with these commands. A change to one fails here.
    #[test]
    fn scheduled_workflow_runs_the_sweep_and_the_model_check() {
        let text = workflow("scheduled.yml");
        assert_runs(
            "scheduled.yml",
            &text,
            &[
                "cargo deny check advisories",
                "cargo test --release --locked -p waveport-domain --test simulation",
                "cargo test --release --locked -p waveport-runtime --test simulation",
                "cargo test --release --locked -p waveport-store-journal --test power",
                "WAVEPORT_SIM_SEEDS=",
                "cargo test --release --locked -p waveport-domain --test model -- --include-ignored",
            ],
        );
    }

    /// The crates whose tests named `live_*` talk to a provider, by the
    /// names of their packages: every adapter for speech recognition, a
    /// language model or speech synthesis under `crates/adapters` whose
    /// tests or sources define one. The sound devices have tests of that
    /// name too, which need a machine with a loudspeaker and no key.
    fn packages_with_live_tests() -> Vec<String> {
        fn holds_one(folder: &Path) -> bool {
            let Ok(entries) = std::fs::read_dir(folder) else {
                return false;
            };
            entries.flatten().any(|entry| {
                let path = entry.path();
                if path.is_dir() {
                    return holds_one(&path);
                }
                path.extension().is_some_and(|extension| extension == "rs")
                    && std::fs::read_to_string(&path)
                        .is_ok_and(|text| text.contains("async fn live_"))
            })
        }
        let adapters = Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/adapters");
        let mut packages = Vec::new();
        for kind in ["stt", "llm", "tts"] {
            for adapter in std::fs::read_dir(adapters.join(kind)).unwrap().flatten() {
                let folder = adapter.path();
                if !holds_one(&folder.join("tests")) && !holds_one(&folder.join("src")) {
                    continue;
                }
                let manifest = std::fs::read_to_string(folder.join("Cargo.toml")).unwrap();
                let name = manifest
                    .lines()
                    .find_map(|line| line.strip_prefix("name = \""))
                    .and_then(|rest| rest.strip_suffix('"'))
                    .unwrap();
                packages.push(name.to_owned());
            }
        }
        packages.sort();
        packages
    }

    /// The tests that talk to the providers run with real keys. They run
    /// every night and by hand, for every adapter that has some, and
    /// never for a pull request: code that nobody has reviewed would
    /// have the keys in reach.
    #[test]
    fn live_workflow_runs_every_live_test_and_never_for_a_pull_request() {
        let text = workflow("live.yml");
        assert_runs(
            "live.yml",
            &text,
            &["cargo test --locked -p \"${PACKAGE}\" -- --ignored live_"],
        );
        let packages = packages_with_live_tests();
        assert!(packages.len() >= 6, "{packages:?}");
        for package in &packages {
            assert!(
                text.contains(&format!("- package: {package} ")),
                "`.github/workflows/live.yml` does not run the live tests of `{package}`"
            );
        }
        // What starts it: the clock, and somebody who asks.
        let triggers = text
            .split_once(" on: ")
            .and_then(|(_, rest)| rest.split_once(" permissions:"))
            .map(|(triggers, _)| triggers)
            .unwrap();
        assert_eq!(
            triggers,
            "schedule: - cron: \"43 4 * * *\" workflow_dispatch:"
        );
        // A key is in reach of the one step that runs the tests, and of
        // the one that looks whether there is a key at all.
        assert_eq!(text.matches("secrets[").count(), 2);
        assert!(!text.contains("pull_request"));
    }

    /// No other workflow has a provider's key in reach.
    #[test]
    fn no_other_workflow_reads_a_providers_key() {
        for name in ["ci.yml", "pr-meta.yml", "scheduled.yml", "release-plz.yml"] {
            let text = workflow(name);
            for key in ["_API_KEY", "secrets["] {
                assert!(
                    !text.contains(key),
                    "`.github/workflows/{name}` reads `{key}`"
                );
            }
        }
    }

    #[test]
    fn pull_request_workflow_runs_the_title_and_semver_gates() {
        let text = workflow("pr-meta.yml");
        assert_runs(
            "pr-meta.yml",
            &text,
            &[
                "cargo xtask check-title --env PR_TITLE",
                "cargo xtask semver --baseline-rev",
                "--title-env PR_TITLE",
            ],
        );
        // Code from a fork must never run with a privileged token.
        assert!(!text.contains("pull_request_target:"));
    }
}
