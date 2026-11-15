//! `cargo xtask semver`: the semver gate, scaled by stability tier.
//!
//! - `stable` crates may not break their API outside a major release.
//! - `preview` crates may, but only when the pull-request title declares it
//!   with `!`.
//! - `experimental` and `internal` crates are not checked.
//!
//! `--self-test` runs the underlying tool on three tiny crates and requires
//! it to accept a compatible change and reject a breaking one. It keeps the
//! gate proven while no crate is stable enough to be checked.

use std::path::Path;

use crate::args::Args;
use crate::error::{Context, Error, Result};
use crate::meta::{self, LoadOptions, Workspace};
use crate::tier::Tier;
use crate::{git, title};

/// Exit code `cargo semver-checks` uses for "the API broke".
const BROKEN: i32 = 100;

const FIXTURE_MANIFEST: &str = "\
[package]
name = \"semver-fixture\"
version = \"1.0.0\"
edition = \"2024\"

[workspace]
";

const FIXTURE_KEPT: &str = "\
//! Semver gate fixture.

/// Present in every variant.
pub fn kept() -> u32 {
    1
}
";

const FIXTURE_DROPPED: &str = "
/// Missing from the breaking variant.
pub fn dropped_later() -> u32 {
    2
}
";

const FIXTURE_ADDED: &str = "
/// Present only in the compatible variant.
pub fn added() -> u32 {
    3
}
";

/// Runs the gate, or its self-test.
pub(crate) fn run(args: &[String]) -> Result<bool> {
    let mut args = Args::new(args);
    let self_test = args.flag("--self-test");
    let baseline = args.value("--baseline-rev")?;
    let inline_title = args.value("--title")?;
    let title_variable = args.value("--title-env")?;
    args.finish()?;
    let workspace = meta::load(&LoadOptions::default())?;
    if self_test {
        return run_self_test(&workspace);
    }
    let Some(baseline) = baseline else {
        return Err(Error::new(
            "usage: cargo xtask semver --self-test | --baseline-rev <rev> [--title <title> | --title-env <VARIABLE>]",
        ));
    };
    let breaking_declared = match title::from_args(inline_title, title_variable)? {
        Some(text) => {
            title::parse(&text, &title::scopes(&workspace.graph))
                .map_err(|reason| Error::new(format!("invalid title: {reason}")))?
                .breaking
        }
        None => false,
    };
    check(&workspace, &baseline, breaking_declared)
}

/// Compares every stable and preview crate with `baseline`.
pub(crate) fn check(
    workspace: &Workspace,
    baseline: &str,
    breaking_declared: bool,
) -> Result<bool> {
    let mut checked = 0_usize;
    let mut broken = Vec::new();
    let mut exempt = 0_usize;
    for (_, package, member) in workspace.graph.members() {
        let tier = member.stability.as_deref().and_then(Tier::parse);
        let Some(tier @ (Tier::Stable | Tier::Preview)) = tier else {
            exempt += 1;
            continue;
        };
        if !git::exists_at(&workspace.root, baseline, &member.manifest())? {
            println!(
                "semver: `{}` does not exist at {baseline}; nothing to compare",
                package.name
            );
            continue;
        }
        let mut command = meta::cargo();
        command.current_dir(&workspace.root).args([
            "semver-checks",
            "-p",
            &package.name,
            "--baseline-rev",
            baseline,
        ]);
        if tier == Tier::Preview && breaking_declared {
            command.args(["--release-type", "major"]);
        }
        let status = command
            .status()
            .context("could not run `cargo semver-checks`")?;
        match status.code() {
            Some(0) => {}
            Some(BROKEN) => broken.push(package.name.as_str()),
            _ => {
                return Err(Error::new(format!(
                    "`cargo semver-checks` could not check `{}`",
                    package.name
                )));
            }
        }
        checked += 1;
    }
    println!(
        "semver: {checked} crates checked against {baseline}, \
         {exempt} experimental or internal crates exempt"
    );
    if broken.is_empty() {
        return Ok(true);
    }
    eprintln!(
        "error[SEMVER001]: breaking API change in: {}",
        broken.join(", ")
    );
    eprintln!("   = rule: stable crates break only in a major release; preview crates only");
    eprintln!("           when the pull-request title declares it with `!`");
    Ok(false)
}

/// Proves that the tool accepts a compatible change and rejects a breaking one.
fn run_self_test(workspace: &Workspace) -> Result<bool> {
    let dir = workspace.target_dir.join("xtask").join("semver-self-test");
    let base = write_fixture(&dir, "base", &[FIXTURE_KEPT, FIXTURE_DROPPED])?;
    let cases = [
        (
            "compatible",
            [FIXTURE_KEPT, FIXTURE_DROPPED, FIXTURE_ADDED].as_slice(),
            0,
        ),
        ("breaking", [FIXTURE_KEPT].as_slice(), BROKEN),
    ];
    let mut passed = true;
    for (variant, sources, expected) in cases {
        let current = write_fixture(&dir, variant, sources)?;
        let output = meta::cargo()
            .arg("semver-checks")
            .arg("--manifest-path")
            .arg(current.join("Cargo.toml"))
            .arg("--baseline-root")
            .arg(&base)
            .output()
            .context("could not run `cargo semver-checks`; is cargo-semver-checks installed?")?;
        let code = output.status.code();
        if code == Some(expected) {
            println!(
                "semver self-test: `{variant}` change gives exit code {expected}, as required"
            );
        } else {
            passed = false;
            eprintln!(
                "error[SEMVER002]: `{variant}` change gave exit code {code:?}, expected {expected}"
            );
            eprintln!("{}", String::from_utf8_lossy(&output.stderr).trim_end());
        }
    }
    Ok(passed)
}

fn write_fixture(dir: &Path, variant: &str, sources: &[&str]) -> Result<std::path::PathBuf> {
    let root = dir.join(variant);
    let src = root.join("src");
    std::fs::create_dir_all(&src).context(format!("could not create {}", src.display()))?;
    std::fs::write(root.join("Cargo.toml"), FIXTURE_MANIFEST)
        .context("could not write the fixture manifest")?;
    std::fs::write(src.join("lib.rs"), sources.concat())
        .context("could not write the fixture source")?;
    Ok(root)
}
