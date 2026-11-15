//! `cargo xtask lint-sync`: keeps lint configuration from drifting.
//!
//! The workspace forbids `unsafe` code, and `forbid` cannot be lowered. A
//! crate that needs `unsafe` therefore cannot inherit the workspace lint
//! table: it carries a copy with `unsafe_code = "deny"`. This gate checks
//! that the copy stays equal to the original and that only allow-listed
//! crates have one. It also checks nested `clippy.toml` files, which replace
//! the root file instead of merging with it.

use std::collections::BTreeSet;
use std::path::Path;

use toml::{Table, Value};

use crate::args::Args;
use crate::error::{Context, Result};
use crate::meta::{self, LoadOptions};
use crate::report::{self, Violation};

/// The configuration files of one workspace member.
pub(crate) struct MemberFiles {
    pub(crate) name: String,
    /// Directory relative to the workspace root.
    pub(crate) dir: String,
    pub(crate) manifest: Table,
    /// The member's own `clippy.toml`, when it has one.
    pub(crate) clippy: Option<Table>,
}

/// Runs the gate on the workspace in the current directory.
pub(crate) fn run(args: &[String]) -> Result<bool> {
    Args::new(args).finish()?;
    let workspace = meta::load(&LoadOptions::default())?;
    let root_manifest = read_table(&workspace.root.join("Cargo.toml"))?;
    let root_clippy_path = workspace.root.join("clippy.toml");
    let root_clippy = if root_clippy_path.is_file() {
        read_table(&root_clippy_path)?
    } else {
        Table::new()
    };
    let mut members = Vec::new();
    for (_, package, member) in workspace.graph.members() {
        let dir = workspace.root.join(&member.dir);
        let clippy = dir.join("clippy.toml");
        members.push(MemberFiles {
            name: package.name.clone(),
            dir: member.dir.clone(),
            manifest: read_table(&dir.join("Cargo.toml"))?,
            clippy: if clippy.is_file() {
                Some(read_table(&clippy)?)
            } else {
                None
            },
        });
    }
    let violations = check(
        &root_manifest,
        &root_clippy,
        &workspace.policy.unsafe_allowed,
        &members,
    );
    Ok(report::finish("lint-sync", &violations, members.len()))
}

fn read_table(path: &Path) -> Result<Table> {
    let text =
        std::fs::read_to_string(path).context(format!("could not read {}", path.display()))?;
    text.parse::<Table>()
        .context(format!("could not parse {}", path.display()))
}

/// Checks every lint-configuration rule.
pub(crate) fn check(
    root_manifest: &Table,
    root_clippy: &Table,
    unsafe_allowed: &BTreeSet<String>,
    members: &[MemberFiles],
) -> Vec<Violation> {
    let mut out = Vec::new();
    let workspace_lints = root_manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("lints"))
        .and_then(Value::as_table)
        .cloned()
        .unwrap_or_default();
    if lint(&workspace_lints, "rust", "unsafe_code") != Some("forbid") {
        out.push(
            Violation::new(
                "LINT005",
                "the workspace does not forbid `unsafe` code",
                "Cargo.toml",
            )
            .note("rule", "`[workspace.lints.rust] unsafe_code` is \"forbid\"")
            .note(
                "help",
                "crates that need `unsafe` opt out one by one; see CONTRIBUTING.md",
            ),
        );
    }
    let mut expected_copy = workspace_lints;
    if let Some(Value::Table(rust)) = expected_copy.get_mut("rust") {
        rust.insert("unsafe_code".to_owned(), Value::String("deny".to_owned()));
    }

    for member in members {
        inheritance(
            member,
            &expected_copy,
            unsafe_allowed.contains(&member.name),
            &mut out,
        );
        nested_clippy(member, root_clippy, &mut out);
    }

    for name in unsafe_allowed {
        if !members.iter().any(|member| &member.name == name) {
            out.push(
                Violation::new(
                    "LINT003",
                    format!("`{name}` is allowed `unsafe` but is not a workspace crate"),
                    "Cargo.toml",
                )
                .note(
                    "help",
                    "remove it from `workspace.metadata.waveport.unsafe.allowed`",
                ),
            );
        }
    }
    out
}

/// LINT001, LINT002 and LINT003: who may opt out of the workspace lints, and how.
fn inheritance(
    member: &MemberFiles,
    expected_copy: &Table,
    allowed: bool,
    out: &mut Vec<Violation>,
) {
    let manifest = format!("{}/Cargo.toml", member.dir);
    let lints = member.manifest.get("lints").and_then(Value::as_table);
    let inherits = lints
        .and_then(|lints| lints.get("workspace"))
        .and_then(Value::as_bool)
        == Some(true);
    match (inherits, allowed) {
        (true, false) => {}
        (true, true) => out.push(
            Violation::new(
                "LINT003",
                format!(
                    "`{}` is allowed `unsafe` but inherits the workspace lints, which forbid it",
                    member.name
                ),
                "Cargo.toml",
            )
            .note(
                "help",
                "remove it from `workspace.metadata.waveport.unsafe.allowed`",
            ),
        ),
        (false, false) => out.push(
            Violation::new(
                "LINT001",
                format!("`{}` does not inherit the workspace lints", member.name),
                &manifest,
            )
            .note(
                "rule",
                "every crate has `[lints] workspace = true` unless it is allowed `unsafe`",
            )
            .note(
                "help",
                "inherit the lints, or add the crate to \
                 `workspace.metadata.waveport.unsafe.allowed` after review",
            ),
        ),
        (false, true) => {
            let differences = differences(expected_copy, lints.unwrap_or(&Table::new()));
            if !differences.is_empty() {
                out.push(
                    Violation::new(
                        "LINT002",
                        format!(
                            "the lint table of `{}` has drifted from the workspace's",
                            member.name
                        ),
                        &manifest,
                    )
                    .note("differs", differences.join("; "))
                    .note(
                        "rule",
                        "the copy equals `[workspace.lints]` except `rust.unsafe_code = \"deny\"`",
                    ),
                );
            }
        }
    }
}

/// LINT004: a nested `clippy.toml` must repeat every root setting.
fn nested_clippy(member: &MemberFiles, root_clippy: &Table, out: &mut Vec<Violation>) {
    let Some(clippy) = &member.clippy else {
        return;
    };
    let missing: Vec<String> = root_clippy
        .iter()
        .filter(|(key, value)| clippy.get(key.as_str()) != Some(value))
        .map(|(key, value)| format!("{key} = {value}"))
        .collect();
    if !missing.is_empty() {
        out.push(
            Violation::new(
                "LINT004",
                format!(
                    "`{}/clippy.toml` does not repeat the workspace settings",
                    member.dir
                ),
                &format!("{}/clippy.toml", member.dir),
            )
            .note("missing", missing.join("; "))
            .note(
                "rule",
                "a nested `clippy.toml` replaces the root file, so it repeats every root key",
            ),
        );
    }
}

fn lint<'t>(lints: &'t Table, tool: &str, name: &str) -> Option<&'t str> {
    lints.get(tool)?.get(name)?.as_str()
}

/// Lists the lints whose levels differ between two lint tables.
fn differences(expected: &Table, actual: &Table) -> Vec<String> {
    let mut out = Vec::new();
    let tools: BTreeSet<&String> = expected.keys().chain(actual.keys()).collect();
    for tool in tools {
        let empty = Table::new();
        let table = |source: &'_ Table| -> Table {
            source
                .get(tool.as_str())
                .and_then(Value::as_table)
                .cloned()
                .unwrap_or_else(|| empty.clone())
        };
        let (want, have) = (table(expected), table(actual));
        let names: BTreeSet<&String> = want.keys().chain(have.keys()).collect();
        for name in names {
            let (want, have) = (want.get(name.as_str()), have.get(name.as_str()));
            if want != have {
                let show = |value: Option<&Value>| {
                    value.map_or_else(|| "nothing".to_owned(), ToString::to_string)
                };
                out.push(format!(
                    "{tool}.{name}: expected {}, found {}",
                    show(want),
                    show(have)
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use toml::Table;

    use super::{MemberFiles, check};

    const ROOT: &str = r#"
        [workspace.lints.rust]
        unsafe_code = "forbid"
        missing_docs = "warn"

        [workspace.lints.clippy]
        pedantic = { level = "warn", priority = -1 }
    "#;

    const ROOT_CLIPPY: &str = "allow-unwrap-in-tests = true\n";

    fn table(text: &str) -> Table {
        text.parse().unwrap()
    }

    fn member(name: &str, manifest: &str, clippy: Option<&str>) -> MemberFiles {
        MemberFiles {
            name: name.to_owned(),
            dir: format!("crates/{name}"),
            manifest: table(manifest),
            clippy: clippy.map(table),
        }
    }

    fn codes(root: &str, allowed: &[&str], members: &[MemberFiles]) -> Vec<&'static str> {
        let allowed: BTreeSet<String> = allowed.iter().map(|name| (*name).to_owned()).collect();
        check(&table(root), &table(ROOT_CLIPPY), &allowed, members)
            .iter()
            .map(|violation| violation.code)
            .collect()
    }

    const INHERITS: &str = "[lints]\nworkspace = true\n";
    const COPY: &str = r#"
        [lints.rust]
        unsafe_code = "deny"
        missing_docs = "warn"

        [lints.clippy]
        pedantic = { level = "warn", priority = -1 }
    "#;

    #[test]
    fn inheriting_crates_and_a_faithful_copy_pass() {
        let members = [
            member("plain", INHERITS, None),
            member("raw", COPY, Some(ROOT_CLIPPY)),
        ];
        assert_eq!(codes(ROOT, &["raw"], &members), Vec::<&str>::new());
    }

    #[test]
    fn lint001_crate_opts_out_without_being_allowed() {
        assert_eq!(codes(ROOT, &[], &[member("raw", COPY, None)]), ["LINT001"]);
        assert_eq!(codes(ROOT, &[], &[member("bare", "", None)]), ["LINT001"]);
    }

    #[test]
    fn lint002_copy_drifts() {
        let relaxed = COPY.replace("\"deny\"", "\"allow\"");
        assert_eq!(
            codes(ROOT, &["raw"], &[member("raw", &relaxed, None)]),
            ["LINT002"]
        );
        let dropped = COPY.replace("missing_docs = \"warn\"", "");
        assert_eq!(
            codes(ROOT, &["raw"], &[member("raw", &dropped, None)]),
            ["LINT002"]
        );
    }

    #[test]
    fn lint003_stale_allow_list_entry() {
        assert_eq!(
            codes(ROOT, &["gone"], &[member("plain", INHERITS, None)]),
            ["LINT003"]
        );
        assert_eq!(
            codes(ROOT, &["plain"], &[member("plain", INHERITS, None)]),
            ["LINT003"]
        );
    }

    #[test]
    fn lint004_nested_clippy_config_drops_a_root_key() {
        let members = [member("plain", INHERITS, Some("disallowed-types = []\n"))];
        assert_eq!(codes(ROOT, &[], &members), ["LINT004"]);
    }

    #[test]
    fn lint005_workspace_stops_forbidding_unsafe() {
        let relaxed = ROOT.replace("\"forbid\"", "\"deny\"");
        assert_eq!(
            codes(&relaxed, &[], &[member("plain", INHERITS, None)]),
            ["LINT005"]
        );
    }
}
