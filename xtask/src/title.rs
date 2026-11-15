//! `cargo xtask check-title`: the changelog convention.
//!
//! Pull requests are squash-merged with their title as the commit subject,
//! and the changelog is generated from those subjects. A title is therefore
//! a Conventional Commit: `type(scope)!: subject`.

use std::collections::BTreeSet;

use crate::args::Args;
use crate::error::{Context, Error, Result};
use crate::meta::{self, Graph, LoadOptions};

/// Commit types, in the order the changelog groups them.
pub(crate) const TYPES: &[&str] = &[
    "feat", "fix", "perf", "refactor", "docs", "test", "build", "ci", "chore", "revert",
];

/// Scopes that are not crate names.
const EXTRA_SCOPES: &[&str] = &["ci", "deps", "release", "docs"];

/// Prefix dropped from crate names to form their scope.
const CRATE_PREFIX: &str = "waveport-";

/// A parsed pull-request title.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Title {
    pub(crate) kind: String,
    pub(crate) scope: Option<String>,
    /// Whether the title declares a breaking change with `!`.
    pub(crate) breaking: bool,
    pub(crate) subject: String,
}

/// The scopes a title may use: every crate name without its prefix, plus a few fixed ones.
pub(crate) fn scopes(graph: &Graph) -> BTreeSet<String> {
    graph
        .members()
        .map(|(_, package, _)| {
            package
                .name
                .strip_prefix(CRATE_PREFIX)
                .unwrap_or(&package.name)
                .to_owned()
        })
        .chain(EXTRA_SCOPES.iter().map(|scope| (*scope).to_owned()))
        .collect()
}

/// Parses a title, or says what is wrong with it.
pub(crate) fn parse(title: &str, scopes: &BTreeSet<String>) -> std::result::Result<Title, String> {
    let (head, subject) = title
        .split_once(": ")
        .ok_or("expected `type(scope): subject`")?;
    let (head, breaking) = match head.strip_suffix('!') {
        Some(rest) => (rest, true),
        None => (head, false),
    };
    let (kind, scope) = match head.split_once('(') {
        Some((kind, rest)) => {
            let scope = rest
                .strip_suffix(')')
                .ok_or("the scope is missing its closing `)`")?;
            (kind, Some(scope))
        }
        None => (head, None),
    };
    if !TYPES.contains(&kind) {
        return Err(format!(
            "unknown type `{kind}`; use one of: {}",
            TYPES.join(", ")
        ));
    }
    if let Some(scope) = scope
        && !scopes.contains(scope)
    {
        let known = scopes.iter().map(String::as_str).collect::<Vec<_>>();
        return Err(format!(
            "unknown scope `{scope}`; use one of: {}",
            known.join(", ")
        ));
    }
    if subject.is_empty() || subject.trim() != subject {
        return Err("the subject must not be empty or padded with spaces".to_owned());
    }
    if subject.ends_with('.') {
        return Err("the subject must not end with a full stop".to_owned());
    }
    Ok(Title {
        kind: kind.to_owned(),
        scope: scope.map(str::to_owned),
        breaking,
        subject: subject.to_owned(),
    })
}

/// Reads a title from an argument or an environment variable.
///
/// Workflows pass the title through the environment so that it is never
/// interpolated into a shell command.
pub(crate) fn from_args(
    inline: Option<String>,
    variable: Option<String>,
) -> Result<Option<String>> {
    match (inline, variable) {
        (Some(_), Some(_)) => Err(Error::new(
            "give the title either inline or through `--env`, not both",
        )),
        (Some(title), None) => Ok(Some(title)),
        (None, Some(name)) => std::env::var(&name)
            .context(format!("could not read environment variable `{name}`"))
            .map(Some),
        (None, None) => Ok(None),
    }
}

/// Runs the gate on one title.
pub(crate) fn run(args: &[String]) -> Result<bool> {
    let mut args = Args::new(args);
    let variable = args.value("--env")?;
    let mut rest = args.finish()?;
    if rest.len() > 1 {
        return Err(Error::new(
            "usage: cargo xtask check-title <title> | --env <VARIABLE>",
        ));
    }
    let Some(title) = from_args(rest.pop(), variable)? else {
        return Err(Error::new(
            "usage: cargo xtask check-title <title> | --env <VARIABLE>",
        ));
    };
    let workspace = meta::load(&LoadOptions::default())?;
    match parse(&title, &scopes(&workspace.graph)) {
        Ok(parsed) => {
            let breaking = if parsed.breaking { ", breaking" } else { "" };
            println!("check-title: ok ({}{breaking})", parsed.kind);
            Ok(true)
        }
        Err(reason) => {
            eprintln!("error[TITLE001]: {reason}");
            eprintln!("   = title: {title}");
            eprintln!("   = rule: titles follow `type(scope)!: subject`; see CONTRIBUTING.md");
            if std::env::var("GITHUB_ACTIONS").is_ok_and(|value| value == "true") {
                println!("::error title=TITLE001::{reason}");
            }
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{Title, parse};

    fn scopes() -> BTreeSet<String> {
        ["domain", "xtask", "ci", "deps", "release", "docs"]
            .iter()
            .map(|scope| (*scope).to_owned())
            .collect()
    }

    fn title(kind: &str, scope: Option<&str>, breaking: bool, subject: &str) -> Title {
        Title {
            kind: kind.to_owned(),
            scope: scope.map(str::to_owned),
            breaking,
            subject: subject.to_owned(),
        }
    }

    #[test]
    fn valid_titles_parse() {
        let cases = [
            (
                "feat: add the session kernel",
                title("feat", None, false, "add the session kernel"),
            ),
            (
                "fix(domain): commit spoken text once",
                title("fix", Some("domain"), false, "commit spoken text once"),
            ),
            (
                "feat(domain)!: rename Epoch",
                title("feat", Some("domain"), true, "rename Epoch"),
            ),
            (
                "refactor!: drop the old ports",
                title("refactor", None, true, "drop the old ports"),
            ),
            (
                "chore(release): release",
                title("chore", Some("release"), false, "release"),
            ),
            (
                "build(deps): bump serde",
                title("build", Some("deps"), false, "bump serde"),
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(parse(text, &scopes()).as_ref(), Ok(&expected), "{text}");
        }
    }

    #[test]
    fn invalid_titles_are_rejected() {
        let cases = [
            "add the session kernel",
            "feature: add the session kernel",
            "feat(kernel): add the session kernel",
            "feat(domain: add the session kernel",
            "feat:add the session kernel",
            "feat: ",
            "feat:  padded",
            "feat: ends with a full stop.",
            "Feat: wrong case",
        ];
        for text in cases {
            assert!(parse(text, &scopes()).is_err(), "{text}");
        }
    }
}
