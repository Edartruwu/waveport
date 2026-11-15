//! `cargo xtask arch-check`: enforces the dependency rule between layers.
//!
//! Each workspace crate declares its layer under `[package.metadata.waveport]`.
//! The rules below are checked against the graph `cargo metadata` resolves
//! with every feature on and for every platform, so the check can report a
//! dependency that only one configuration has, but can never miss one.

use std::collections::{BTreeMap, VecDeque};

use crate::args::Args;
use crate::error::Result;
use crate::meta::{self, Edge, Graph, LoadOptions, Member, Package, Policy};
use crate::report::{self, Violation};

/// The layer a workspace crate belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Layer {
    Domain,
    Ports,
    Protocol,
    Application,
    Adapter,
    Composition,
    Testing,
    Tooling,
}

const ALL: &[Layer] = &[
    Layer::Domain,
    Layer::Ports,
    Layer::Protocol,
    Layer::Application,
    Layer::Adapter,
    Layer::Composition,
    Layer::Testing,
    Layer::Tooling,
];

impl Layer {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        ALL.iter().copied().find(|layer| layer.name() == text)
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Domain => "domain",
            Self::Ports => "ports",
            Self::Protocol => "protocol",
            Self::Application => "application",
            Self::Adapter => "adapter",
            Self::Composition => "composition",
            Self::Testing => "testing",
            Self::Tooling => "tooling",
        }
    }

    /// Layers a crate of this layer may depend on in the code it ships.
    fn may_depend_on(self) -> &'static [Layer] {
        match self {
            Self::Domain | Self::Ports => &[Self::Domain],
            Self::Protocol => &[],
            Self::Application => &[Self::Domain, Self::Ports, Self::Protocol, Self::Application],
            Self::Adapter => &[Self::Domain, Self::Ports, Self::Protocol],
            Self::Composition => &[
                Self::Domain,
                Self::Ports,
                Self::Protocol,
                Self::Application,
                Self::Adapter,
                Self::Composition,
            ],
            Self::Testing => &[
                Self::Domain,
                Self::Ports,
                Self::Protocol,
                Self::Application,
                Self::Adapter,
                Self::Composition,
                Self::Testing,
            ],
            Self::Tooling => ALL,
        }
    }

    /// The layers that must stay free of runtimes and unreviewed dependencies.
    fn is_inner(self) -> bool {
        matches!(self, Self::Domain | Self::Ports | Self::Protocol)
    }

    /// Layers that exist to build and test the product, not to be part of it.
    fn is_support(self) -> bool {
        matches!(self, Self::Testing | Self::Tooling)
    }
}

/// Layers a crate may declare when its manifest lives in `dir`.
fn layers_for_dir(dir: &str) -> &'static [Layer] {
    if dir == "xtask" || dir == "benches" {
        return &[Layer::Tooling];
    }
    if let Some(name) = dir.strip_prefix("crates/core/") {
        if name.contains('/') {
            return &[];
        }
        return &[
            Layer::Domain,
            Layer::Ports,
            Layer::Protocol,
            Layer::Application,
        ];
    }
    if dir.starts_with("crates/adapters/") {
        return &[Layer::Adapter];
    }
    if dir.starts_with("crates/testing/") {
        return &[Layer::Testing];
    }
    if dir.starts_with("apps/") || dir.starts_with("crates/bindings/") {
        return &[Layer::Composition];
    }
    &[]
}

fn list(layers: &[Layer]) -> String {
    if layers.is_empty() {
        return "no workspace crate".to_owned();
    }
    layers
        .iter()
        .map(|layer| layer.name())
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
    let violations = check(&workspace.graph, &workspace.policy);
    Ok(report::finish(
        "arch-check",
        &violations,
        workspace.graph.member_count(),
    ))
}

/// Checks every architecture rule and returns what is broken.
pub(crate) fn check(graph: &Graph, policy: &Policy) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut layers: BTreeMap<&str, Layer> = BTreeMap::new();
    for (id, package, member) in graph.members() {
        if let Some(layer) = declared_layer(package, member, &mut out) {
            layers.insert(id, layer);
        }
    }
    for (id, package, member) in graph.members() {
        let Some(&layer) = layers.get(id) else {
            continue;
        };
        let subject = Subject {
            id,
            package,
            member,
            layer,
        };
        direct_edges(graph, policy, &layers, &subject, &mut out);
        if layer.is_inner() {
            no_runtime(graph, policy, &subject, &mut out);
        }
        if layer == Layer::Adapter {
            no_other_adapter(graph, &layers, &subject, &mut out);
        }
        if matches!(layer, Layer::Domain | Layer::Ports) && member.has_build_script {
            out.push(
                Violation::new(
                    "ARCH007",
                    format!(
                        "{} crate `{}` has a build script",
                        layer.name(),
                        package.name
                    ),
                    &member.manifest(),
                )
                .note(
                    "rule",
                    "domain and ports crates build without running any code",
                )
                .note(
                    "help",
                    "generate the code ahead of time, or move it to an outer layer",
                ),
            );
        }
    }
    out
}

/// A workspace member being checked.
struct Subject<'g> {
    id: &'g str,
    package: &'g Package,
    member: &'g Member,
    layer: Layer,
}

/// ARCH008: the layer must be declared, known, and match where the crate lives.
fn declared_layer(package: &Package, member: &Member, out: &mut Vec<Violation>) -> Option<Layer> {
    let manifest = member.manifest();
    let Some(text) = member.layer.as_deref() else {
        out.push(
            Violation::new(
                "ARCH008",
                format!(
                    "`{}` does not declare `package.metadata.waveport.layer`",
                    package.name
                ),
                &manifest,
            )
            .note(
                "help",
                format!("add `layer = \"…\"`; valid layers: {}", list(ALL)),
            ),
        );
        return None;
    };
    let Some(layer) = Layer::parse(text) else {
        out.push(
            Violation::new(
                "ARCH008",
                format!("`{}` declares unknown layer `{text}`", package.name),
                &manifest,
            )
            .note("help", format!("valid layers: {}", list(ALL))),
        );
        return None;
    };
    let allowed = layers_for_dir(&member.dir);
    if !allowed.contains(&layer) {
        let rule = if allowed.is_empty() {
            format!(
                "no crate may live in `{}`; see the workspace layout",
                member.dir
            )
        } else {
            format!(
                "crates in `{}` must be one of: {}",
                member.dir,
                list(allowed)
            )
        };
        out.push(
            Violation::new(
                "ARCH008",
                format!(
                    "`{}` declares layer `{}` but lives in `{}`",
                    package.name,
                    layer.name(),
                    member.dir
                ),
                &manifest,
            )
            .note("rule", rule)
            .note(
                "help",
                "move the crate, or declare the layer its location implies",
            ),
        );
    }
    Some(layer)
}

/// ARCH001, ARCH004, ARCH005 and ARCH006: rules about direct dependencies.
fn direct_edges(
    graph: &Graph,
    policy: &Policy,
    layers: &BTreeMap<&str, Layer>,
    subject: &Subject<'_>,
    out: &mut Vec<Violation>,
) {
    for edge in &subject.package.edges {
        let Some(target) = graph.packages.get(&edge.to) else {
            continue;
        };
        if let Some(&target_layer) = layers.get(edge.to.as_str()) {
            member_edge(subject, edge, target, target_layer, out);
        } else if target.member.is_none() && subject.layer.is_inner() {
            external_edge(policy, subject, edge, target, out);
        }
    }
}

/// ARCH001 and ARCH005: a dependency on another workspace crate.
fn member_edge(
    subject: &Subject<'_>,
    edge: &Edge,
    target: &Package,
    target_layer: Layer,
    out: &mut Vec<Violation>,
) {
    let Subject {
        package,
        member,
        layer,
        ..
    } = subject;
    let layer = *layer;
    let manifest = member.manifest();
    let verb = if edge.is_shipping() {
        "depends on"
    } else {
        "dev-depends on"
    };
    let summary = format!(
        "{} crate `{}` {verb} {} crate `{}`",
        layer.name(),
        package.name,
        target_layer.name(),
        target.name
    );
    if !edge.is_shipping() {
        // Tests may use the testing crates; every other rule still applies.
        if target_layer != Layer::Testing && !layer.may_depend_on().contains(&target_layer) {
            out.push(wrong_direction(summary, &manifest, layer));
        }
    } else if layer == Layer::Adapter && target_layer == Layer::Adapter {
        // Reported with its full path by ARCH003.
    } else if target_layer.is_support() && !layer.is_support() {
        out.push(
            Violation::new("ARCH005", summary, &manifest)
                .note("rule", "testing and tooling crates are never shipped")
                .note("help", "move the dependency to `[dev-dependencies]`"),
        );
    } else if package.name == "xtask" {
        out.push(
            Violation::new("ARCH001", summary, &manifest)
                .note("rule", "xtask depends on no workspace crate")
                .note(
                    "help",
                    "the gates must build even when product crates do not",
                ),
        );
    } else if !layer.may_depend_on().contains(&target_layer) {
        out.push(wrong_direction(summary, &manifest, layer));
    }
}

/// ARCH004 and ARCH006: a dependency of an inner layer on a third-party crate.
fn external_edge(
    policy: &Policy,
    subject: &Subject<'_>,
    edge: &Edge,
    target: &Package,
    out: &mut Vec<Violation>,
) {
    let Subject {
        package,
        member,
        layer,
        ..
    } = subject;
    let manifest = member.manifest();
    let allowed = policy
        .allowed_external
        .get(layer.name())
        .is_some_and(|names| names.contains(&target.name));
    if edge.is_shipping() && !allowed {
        out.push(
            Violation::new(
                "ARCH004",
                format!(
                    "{} crate `{}` depends on third-party crate `{}`, which is not allow-listed",
                    layer.name(),
                    package.name,
                    target.name
                ),
                &manifest,
            )
            .note(
                "rule",
                format!(
                    "third-party dependencies of layer `{}` are listed in \
                     `workspace.metadata.waveport.arch.allowed-external`",
                    layer.name()
                ),
            )
            .note(
                "help",
                "add it to that list after review, or remove the dependency",
            ),
        );
    }
    if edge.is_dev() && policy.runtime_crates.contains(&target.name) {
        out.push(
            Violation::new(
                "ARCH006",
                format!(
                    "{} crate `{}` dev-depends on async runtime `{}`",
                    layer.name(),
                    package.name,
                    target.name
                ),
                &manifest,
            )
            .note(
                "rule",
                format!(
                    "tests of layer `{}` run without an async runtime",
                    layer.name()
                ),
            )
            .note(
                "help",
                "drive the code synchronously, or test it from an outer layer",
            ),
        );
    }
}

fn wrong_direction(summary: String, manifest: &str, layer: Layer) -> Violation {
    Violation::new("ARCH001", summary, manifest)
        .note(
            "rule",
            format!(
                "layer `{}` may depend on: {}",
                layer.name(),
                list(layer.may_depend_on())
            ),
        )
        .note(
            "help",
            "define a port in an inner layer, or move the code to an outer one",
        )
}

/// ARCH002: inner layers never reach a runtime or a source of nondeterminism.
fn no_runtime(graph: &Graph, policy: &Policy, subject: &Subject<'_>, out: &mut Vec<Violation>) {
    let reached = shipping_closure(graph, subject.id);
    for &id in reached.keys() {
        let Some(target) = graph.packages.get(id) else {
            continue;
        };
        let what = if policy.runtime_crates.contains(&target.name) {
            "async runtime"
        } else if policy.nondeterminism_crates.contains(&target.name) {
            "nondeterminism crate"
        } else {
            continue;
        };
        out.push(
            Violation::new(
                "ARCH002",
                format!(
                    "{} crate `{}` reaches {what} `{}`",
                    subject.layer.name(),
                    subject.package.name,
                    target.name
                ),
                &subject.member.manifest(),
            )
            .note("path", path(graph, &reached, subject.id, id))
            .note(
                "rule",
                format!(
                    "layer `{}` must not depend, directly or transitively, on a runtime, \
                     an entropy source or a socket crate",
                    subject.layer.name()
                ),
            )
            .note(
                "help",
                "move the code into waveport-runtime, or put the effect behind a port",
            ),
        );
    }
}

/// ARCH003: an adapter never reaches another adapter.
fn no_other_adapter(
    graph: &Graph,
    layers: &BTreeMap<&str, Layer>,
    subject: &Subject<'_>,
    out: &mut Vec<Violation>,
) {
    let reached = shipping_closure(graph, subject.id);
    for &id in reached.keys() {
        if layers.get(id) != Some(&Layer::Adapter) {
            continue;
        }
        let Some(target) = graph.packages.get(id) else {
            continue;
        };
        out.push(
            Violation::new(
                "ARCH003",
                format!(
                    "adapter `{}` reaches adapter `{}`",
                    subject.package.name, target.name
                ),
                &subject.member.manifest(),
            )
            .note("path", path(graph, &reached, subject.id, id))
            .note("rule", "adapters depend on ports, never on each other")
            .note(
                "help",
                "share the code through waveport-ports or another inner crate",
            ),
        );
    }
}

/// Walks normal and build dependencies from `start`.
///
/// Returns every package reached, mapped to the package it was first reached
/// from. Dev-dependencies are not followed: they are not built for dependents.
fn shipping_closure<'g>(graph: &'g Graph, start: &'g str) -> BTreeMap<&'g str, &'g str> {
    let mut reached_from: BTreeMap<&str, &str> = BTreeMap::new();
    let mut queue = VecDeque::from([start]);
    while let Some(id) = queue.pop_front() {
        let Some(package) = graph.packages.get(id) else {
            continue;
        };
        for edge in package.edges.iter().filter(|edge| edge.is_shipping()) {
            let to = edge.to.as_str();
            if to != start && !reached_from.contains_key(to) {
                reached_from.insert(to, id);
                queue.push_back(to);
            }
        }
    }
    reached_from
}

/// Renders the dependency chain from `start` to `target`.
fn path(graph: &Graph, reached_from: &BTreeMap<&str, &str>, start: &str, target: &str) -> String {
    let mut chain = vec![target];
    let mut cursor = target;
    while cursor != start {
        let Some(&previous) = reached_from.get(cursor) else {
            break;
        };
        chain.push(previous);
        cursor = previous;
    }
    chain
        .iter()
        .rev()
        .map(|&id| match graph.packages.get(id) {
            Some(package) if id == start => package.name.clone(),
            Some(package) => format!("{} {}", package.name, package.version),
            None => id.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(" -> ")
}

#[cfg(test)]
mod tests {
    use super::{Layer, check};
    use crate::meta::DepKind::{Build, Dev, Normal};
    use crate::meta::testing::GraphBuilder;
    use crate::meta::{Graph, Policy};

    fn policy() -> Policy {
        let mut policy = Policy::default();
        policy.runtime_crates.insert("tokio".to_owned());
        policy.nondeterminism_crates.insert("getrandom".to_owned());
        policy
            .allowed_external
            .entry("domain".to_owned())
            .or_default()
            .insert("bytes".to_owned());
        policy
    }

    fn base() -> GraphBuilder {
        GraphBuilder::default()
            .member("dom", "crates/core/dom", "domain")
            .member("ports", "crates/core/ports", "ports")
            .member("proto", "crates/core/proto", "protocol")
            .member("app", "crates/core/app", "application")
            .member("ad-a", "crates/adapters/stt/a", "adapter")
            .member("ad-b", "crates/adapters/tts/b", "adapter")
            .member("kit", "crates/testing/kit", "testing")
            .member("daemon", "apps/daemon", "composition")
            .external("tokio")
            .external("getrandom")
            .external("bytes")
            .external("helper")
    }

    fn codes(graph: &Graph) -> Vec<&'static str> {
        check(graph, &policy())
            .iter()
            .map(|violation| violation.code)
            .collect()
    }

    #[test]
    fn layer_names_round_trip() {
        for layer in super::ALL {
            assert_eq!(Layer::parse(layer.name()), Some(*layer));
        }
        assert_eq!(Layer::parse("core"), None);
    }

    #[test]
    fn clean_layering_passes() {
        let graph = base()
            .edge("ports", "dom", Normal)
            .edge("app", "ports", Normal)
            .edge("app", "proto", Normal)
            .edge("app", "tokio", Normal)
            .edge("ad-a", "ports", Normal)
            .edge("ad-a", "tokio", Normal)
            .edge("daemon", "app", Normal)
            .edge("daemon", "ad-a", Normal)
            .edge("daemon", "ad-b", Normal)
            .edge("dom", "bytes", Normal)
            .edge("dom", "kit", Dev)
            .edge("kit", "tokio", Normal)
            .build();
        assert_eq!(codes(&graph), Vec::<&str>::new());
    }

    #[test]
    fn arch001_inner_layer_depends_outward() {
        assert_eq!(
            codes(&base().edge("dom", "app", Normal).build()),
            ["ARCH001"]
        );
        assert_eq!(
            codes(&base().edge("ports", "proto", Normal).build()),
            ["ARCH001"]
        );
        assert_eq!(
            codes(&base().edge("ad-a", "app", Normal).build()),
            ["ARCH001"]
        );
        assert_eq!(
            codes(&base().edge("proto", "dom", Normal).build()),
            ["ARCH001"]
        );
    }

    #[test]
    fn arch001_applies_to_dev_dependencies_except_on_testing_crates() {
        assert_eq!(codes(&base().edge("dom", "app", Dev).build()), ["ARCH001"]);
        assert_eq!(
            codes(&base().edge("dom", "kit", Dev).build()),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn arch001_xtask_depends_on_no_workspace_crate() {
        let graph = base()
            .member("xtask", "xtask", "tooling")
            .edge("xtask", "dom", Normal)
            .build();
        assert_eq!(codes(&graph), ["ARCH001"]);
    }

    #[test]
    fn arch002_runtime_reached_transitively() {
        let graph = base()
            .edge("dom", "bytes", Normal)
            .edge("bytes", "tokio", Normal)
            .build();
        let violations = check(&graph, &policy());
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].code, "ARCH002");
        let path = &violations[0].notes[0].1;
        assert_eq!(path, "dom -> bytes 1.0.0 -> tokio 1.0.0");
    }

    #[test]
    fn arch002_follows_build_dependencies_and_bans_entropy() {
        let graph = base()
            .edge("dom", "bytes", Build)
            .edge("bytes", "getrandom", Normal)
            .build();
        assert_eq!(codes(&graph), ["ARCH002"]);
    }

    #[test]
    fn arch002_does_not_follow_dev_dependencies() {
        let graph = base()
            .edge("dom", "kit", Dev)
            .edge("kit", "tokio", Normal)
            .build();
        assert_eq!(codes(&graph), Vec::<&str>::new());
    }

    #[test]
    fn arch003_adapter_reaches_adapter() {
        assert_eq!(
            codes(&base().edge("ad-a", "ad-b", Normal).build()),
            ["ARCH003"]
        );
        let indirect = base()
            .edge("ad-a", "daemon", Normal)
            .edge("daemon", "ad-b", Normal)
            .build();
        assert!(codes(&indirect).contains(&"ARCH003"));
    }

    #[test]
    fn arch004_third_party_dependency_not_allow_listed() {
        assert_eq!(
            codes(&base().edge("dom", "helper", Normal).build()),
            ["ARCH004"]
        );
        // `bytes` is allow-listed for the domain layer only.
        assert_eq!(
            codes(&base().edge("ports", "bytes", Normal).build()),
            ["ARCH004"]
        );
        // Outer layers choose their own dependencies.
        assert_eq!(
            codes(&base().edge("app", "helper", Normal).build()),
            Vec::<&str>::new()
        );
        // So do tests.
        assert_eq!(
            codes(&base().edge("dom", "helper", Dev).build()),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn arch005_shipping_crate_depends_on_testing_crate() {
        assert_eq!(
            codes(&base().edge("app", "kit", Normal).build()),
            ["ARCH005"]
        );
        assert_eq!(
            codes(&base().edge("daemon", "kit", Build).build()),
            ["ARCH005"]
        );
    }

    #[test]
    fn arch006_inner_layer_dev_depends_on_runtime() {
        assert_eq!(
            codes(&base().edge("dom", "tokio", Dev).build()),
            ["ARCH006"]
        );
        assert_eq!(
            codes(&base().edge("app", "tokio", Dev).build()),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn arch007_build_script_in_domain_or_ports() {
        let domain = base()
            .edit("dom", |member| member.has_build_script = true)
            .build();
        assert_eq!(codes(&domain), ["ARCH007"]);
        let protocol = base()
            .edit("proto", |member| member.has_build_script = true)
            .build();
        assert_eq!(codes(&protocol), Vec::<&str>::new());
    }

    #[test]
    fn arch008_layer_metadata() {
        let missing = base().edit("dom", |member| member.layer = None).build();
        assert_eq!(codes(&missing), ["ARCH008"]);
        let unknown = base()
            .edit("dom", |member| member.layer = Some("core".to_owned()))
            .build();
        assert_eq!(codes(&unknown), ["ARCH008"]);
        let mismatch = base()
            .member("stray", "crates/core/stray", "adapter")
            .build();
        assert_eq!(codes(&mismatch), ["ARCH008"]);
        let nowhere = base().member("lost", "misc/lost", "domain").build();
        assert_eq!(codes(&nowhere), ["ARCH008"]);
    }
}
