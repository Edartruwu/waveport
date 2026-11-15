//! `cargo xtask tier-check`: enforces the stability tiers.
//!
//! Every workspace crate declares a tier under `[package.metadata.waveport]`.
//! `docs/stability.md` defines what each tier promises.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::args::Args;
use crate::error::Result;
use crate::meta::{self, DepKind, Graph, LoadOptions, Member, Package};
use crate::report::{self, Violation};

/// How much a crate promises about its public API, least stable first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Tier {
    /// Unpublished tooling; not part of the product.
    Internal,
    Experimental,
    Preview,
    Stable,
}

const ALL: &[Tier] = &[
    Tier::Stable,
    Tier::Preview,
    Tier::Experimental,
    Tier::Internal,
];

/// Prefix of a cargo feature that gates experimental items inside a crate.
const UNSTABLE_PREFIX: &str = "unstable-";

impl Tier {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        ALL.iter().copied().find(|tier| tier.name() == text)
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::Experimental => "experimental",
            Self::Preview => "preview",
            Self::Stable => "stable",
        }
    }
}

fn names() -> String {
    ALL.iter()
        .map(|tier| tier.name())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Runs the gate on the workspace in the current directory.
pub(crate) fn run(args: &[String]) -> Result<bool> {
    let mut args = Args::new(args);
    let options = LoadOptions {
        manifest_path: args.value("--manifest-path")?.map(Into::into),
        unlocked: args.flag("--unlocked"),
    };
    args.finish()?;
    let workspace = meta::load(&options)?;
    let violations = check(&workspace.graph, &|path| std::fs::read_to_string(path).ok());
    Ok(report::finish(
        "tier-check",
        &violations,
        workspace.graph.member_count(),
    ))
}

/// Checks every stability rule. `read` returns the text of a source file.
pub(crate) fn check(graph: &Graph, read: &dyn Fn(&Path) -> Option<String>) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut tiers: BTreeMap<&str, Tier> = BTreeMap::new();
    for (_, package, member) in graph.members() {
        if let Some(tier) = declared_tier(package, member, &mut out) {
            tiers.insert(package.name.as_str(), tier);
        }
    }
    for (_, package, member) in graph.members() {
        let Some(&tier) = tiers.get(package.name.as_str()) else {
            continue;
        };
        dependencies(package, member, tier, &tiers, &mut out);
        default_features(package, member, &mut out);
        if tier == Tier::Internal && member.publishable {
            out.push(
                Violation::new(
                    "TIER004",
                    format!("internal crate `{}` can be published", package.name),
                    &member.manifest(),
                )
                .note("rule", "internal crates are never published")
                .note("help", "set `publish = false`"),
            );
        }
        documented(package, member, tier, read, &mut out);
    }
    out
}

/// TIER001: the tier must be declared and known.
fn declared_tier(package: &Package, member: &Member, out: &mut Vec<Violation>) -> Option<Tier> {
    let summary = match member.stability.as_deref() {
        None => format!(
            "`{}` does not declare `package.metadata.waveport.stability`",
            package.name
        ),
        Some(text) => match Tier::parse(text) {
            Some(tier) => return Some(tier),
            None => format!("`{}` declares unknown stability `{text}`", package.name),
        },
    };
    out.push(
        Violation::new("TIER001", summary, &member.manifest())
            .note("help", format!("valid tiers: {}", names())),
    );
    None
}

/// TIER002: a crate never depends on a less stable workspace crate, except
/// through an optional dependency that only `unstable-*` features enable.
fn dependencies(
    package: &Package,
    member: &Member,
    tier: Tier,
    tiers: &BTreeMap<&str, Tier>,
    out: &mut Vec<Violation>,
) {
    for dep in &member.declared {
        if dep.kind == DepKind::Dev {
            continue;
        }
        let Some(&dep_tier) = tiers.get(dep.package.as_str()) else {
            continue;
        };
        if dep_tier >= tier {
            continue;
        }
        let described = format!(
            "{} crate `{}` depends on {} crate `{}`",
            tier.name(),
            package.name,
            dep_tier.name(),
            dep.package
        );
        if !dep.optional {
            out.push(
                Violation::new("TIER002", described, &member.manifest())
                    .note(
                        "rule",
                        "a crate depends only on crates at least as stable as itself",
                    )
                    .note(
                        "help",
                        "lower this crate's tier, raise the dependency's, or make the dependency \
                         optional behind an `unstable-*` feature",
                    ),
            );
            continue;
        }
        for (feature, enables) in &member.features {
            let turns_on = enables.iter().any(|item| {
                item == &dep.feature_name
                    || item.strip_prefix("dep:") == Some(dep.feature_name.as_str())
                    || item
                        .split_once('/')
                        .is_some_and(|(name, _)| name == dep.feature_name)
            });
            if turns_on && !feature.starts_with(UNSTABLE_PREFIX) {
                out.push(
                    Violation::new(
                        "TIER002",
                        format!("{described} through feature `{feature}`"),
                        &member.manifest(),
                    )
                    .note(
                        "rule",
                        "only `unstable-*` features may enable a less stable crate",
                    )
                    .note(
                        "help",
                        format!("rename the feature to `{UNSTABLE_PREFIX}{feature}`"),
                    ),
                );
            }
        }
    }
}

/// TIER003: default features never switch on an `unstable-*` feature.
fn default_features(package: &Package, member: &Member, out: &mut Vec<Violation>) {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut pending = vec!["default"];
    while let Some(feature) = pending.pop() {
        if !seen.insert(feature) {
            continue;
        }
        if feature.starts_with(UNSTABLE_PREFIX) {
            out.push(
                Violation::new(
                    "TIER003",
                    format!("default features of `{}` enable `{feature}`", package.name),
                    &member.manifest(),
                )
                .note("rule", "experimental items are always opt-in")
                .note("help", "remove the feature from `default`"),
            );
        }
        if let Some(enables) = member.features.get(feature) {
            pending.extend(enables.iter().map(String::as_str));
        }
    }
}

/// TIER005: the crate documentation states the tier.
fn documented(
    package: &Package,
    member: &Member,
    tier: Tier,
    read: &dyn Fn(&Path) -> Option<String>,
    out: &mut Vec<Violation>,
) {
    let expected = format!("Stability: {}", tier.name());
    let stated = member
        .root_source
        .as_deref()
        .and_then(read)
        .is_some_and(|source| {
            source
                .lines()
                .filter_map(|line| line.trim_start().strip_prefix("//!"))
                .any(|line| line.trim() == expected)
        });
    if !stated {
        out.push(
            Violation::new(
                "TIER005",
                format!(
                    "the crate documentation of `{}` does not state its tier",
                    package.name
                ),
                &member.manifest(),
            )
            .note(
                "rule",
                "readers of the documentation see the tier the manifest declares",
            )
            .note(
                "help",
                format!("add the line `//! {expected}` to the crate-level documentation"),
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Tier, check};
    use crate::meta::DepKind::{Dev, Normal};
    use crate::meta::Graph;
    use crate::meta::testing::GraphBuilder;

    /// Documentation that satisfies TIER005 for every tier.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "has the signature of the reader `check` takes"
    )]
    fn any_tier(_: &Path) -> Option<String> {
        Some(
            "//! Stability: stable\n//! Stability: preview\n\
             //! Stability: experimental\n//! Stability: internal\n"
                .to_owned(),
        )
    }

    fn base() -> GraphBuilder {
        GraphBuilder::default()
            .member("solid", "crates/core/solid", "domain")
            .edit("solid", |member| {
                member.stability = Some("stable".to_owned());
            })
            .member("young", "crates/core/young", "domain")
    }

    fn codes(graph: &Graph) -> Vec<&'static str> {
        check(graph, &any_tier)
            .iter()
            .map(|violation| violation.code)
            .collect()
    }

    #[test]
    fn tiers_are_ordered_and_named() {
        assert!(Tier::Stable > Tier::Preview);
        assert!(Tier::Preview > Tier::Experimental);
        assert!(Tier::Experimental > Tier::Internal);
        assert_eq!(Tier::parse("preview"), Some(Tier::Preview));
        assert_eq!(Tier::parse("beta"), None);
    }

    #[test]
    fn clean_workspace_passes() {
        let graph = base().edge("young", "solid", Normal).build();
        assert_eq!(codes(&graph), Vec::<&str>::new());
    }

    #[test]
    fn tier001_missing_or_unknown() {
        let missing = base()
            .edit("young", |member| member.stability = None)
            .build();
        assert_eq!(codes(&missing), ["TIER001"]);
        let unknown = base()
            .edit("young", |member| member.stability = Some("beta".to_owned()))
            .build();
        assert_eq!(codes(&unknown), ["TIER001"]);
    }

    #[test]
    fn tier002_stable_depends_on_experimental() {
        let graph = base().edge("solid", "young", Normal).build();
        assert_eq!(codes(&graph), ["TIER002"]);
        let in_tests_only = base().edge("solid", "young", Dev).build();
        assert_eq!(codes(&in_tests_only), Vec::<&str>::new());
    }

    #[test]
    fn tier002_optional_dependency_needs_unstable_feature() {
        let optional = |feature: &'static str| {
            base()
                .edge("solid", "young", Normal)
                .edit("solid", move |member| {
                    member.declared[0].optional = true;
                    member
                        .features
                        .insert(feature.to_owned(), vec!["dep:young".to_owned()]);
                })
                .build()
        };
        assert_eq!(codes(&optional("unstable-young")), Vec::<&str>::new());
        assert_eq!(codes(&optional("young")), ["TIER002"]);
    }

    #[test]
    fn tier003_default_enables_unstable_feature() {
        let graph = base()
            .edit("solid", |member| {
                member
                    .features
                    .insert("default".to_owned(), vec!["extras".to_owned()]);
                member
                    .features
                    .insert("extras".to_owned(), vec!["unstable-probe".to_owned()]);
                member
                    .features
                    .insert("unstable-probe".to_owned(), Vec::new());
            })
            .build();
        assert_eq!(codes(&graph), ["TIER003"]);
    }

    #[test]
    fn tier004_internal_crate_is_publishable() {
        let graph = base()
            .edit("young", |member| {
                member.stability = Some("internal".to_owned());
            })
            .build();
        assert_eq!(codes(&graph), ["TIER004"]);
    }

    #[test]
    fn tier005_documentation_states_the_tier() {
        let graph = GraphBuilder::default()
            .member("young", "crates/core/young", "domain")
            .build();
        let right = |_: &Path| Some("//! A crate.\n//!\n//! Stability: experimental\n".to_owned());
        let wrong = |_: &Path| Some("//! A crate.\n//!\n//! Stability: stable\n".to_owned());
        let unreadable = |_: &Path| None;
        assert!(check(&graph, &right).is_empty());
        assert_eq!(check(&graph, &wrong)[0].code, "TIER005");
        assert_eq!(check(&graph, &unreadable)[0].code, "TIER005");
    }
}
