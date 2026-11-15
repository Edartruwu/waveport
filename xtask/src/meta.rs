//! Loads the workspace through `cargo metadata` into the model the gates check.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::error::{Context, Error, Result};

/// The kind of a dependency edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DepKind {
    Normal,
    Build,
    Dev,
}

impl DepKind {
    fn parse(kind: Option<&str>) -> Self {
        match kind {
            Some("build") => Self::Build,
            Some("dev") => Self::Dev,
            _ => Self::Normal,
        }
    }
}

/// A resolved dependency from one package to another.
#[derive(Debug)]
pub(crate) struct Edge {
    /// Identifier of the package depended on.
    pub(crate) to: String,
    pub(crate) kinds: Vec<DepKind>,
}

impl Edge {
    /// Whether the dependency is compiled into the dependent crate itself,
    /// as opposed to only into its tests.
    pub(crate) fn is_shipping(&self) -> bool {
        self.kinds
            .iter()
            .any(|kind| matches!(kind, DepKind::Normal | DepKind::Build))
    }

    pub(crate) fn is_dev(&self) -> bool {
        self.kinds.contains(&DepKind::Dev)
    }
}

/// A dependency as written in a manifest.
#[derive(Debug)]
pub(crate) struct DeclaredDep {
    /// Name of the package depended on.
    pub(crate) package: String,
    /// Name the dependency has in the `[features]` table.
    pub(crate) feature_name: String,
    pub(crate) kind: DepKind,
    pub(crate) optional: bool,
}

/// What the gates need to know about a workspace member beyond its edges.
#[derive(Debug, Default)]
pub(crate) struct Member {
    /// Directory of the manifest relative to the workspace root, `/`-separated.
    pub(crate) dir: String,
    /// `package.metadata.waveport.layer`, as written.
    pub(crate) layer: Option<String>,
    /// `package.metadata.waveport.stability`, as written.
    pub(crate) stability: Option<String>,
    pub(crate) has_build_script: bool,
    /// False when the manifest says `publish = false`.
    pub(crate) publishable: bool,
    pub(crate) features: BTreeMap<String, Vec<String>>,
    pub(crate) declared: Vec<DeclaredDep>,
    /// Source file holding the crate-level documentation.
    pub(crate) root_source: Option<PathBuf>,
}

impl Member {
    /// Path of the manifest relative to the workspace root.
    pub(crate) fn manifest(&self) -> String {
        format!("{}/Cargo.toml", self.dir)
    }
}

/// One package of the dependency graph.
#[derive(Debug)]
pub(crate) struct Package {
    pub(crate) name: String,
    pub(crate) version: String,
    /// Present for workspace members, absent for third-party packages.
    pub(crate) member: Option<Member>,
    pub(crate) edges: Vec<Edge>,
}

/// Every package in the resolved graph, keyed by package identifier.
#[derive(Debug, Default)]
pub(crate) struct Graph {
    pub(crate) packages: BTreeMap<String, Package>,
}

impl Graph {
    /// Workspace members in a stable order.
    pub(crate) fn members(&self) -> impl Iterator<Item = (&str, &Package, &Member)> {
        self.packages
            .iter()
            .filter_map(|(id, pkg)| pkg.member.as_ref().map(|member| (id.as_str(), pkg, member)))
    }

    pub(crate) fn member_count(&self) -> usize {
        self.members().count()
    }
}

/// The lists under `[workspace.metadata.waveport]` in the root manifest.
#[derive(Debug, Default)]
pub(crate) struct Policy {
    /// Async runtimes and executors the inner layers must never reach.
    pub(crate) runtime_crates: BTreeSet<String>,
    /// Entropy and socket crates the inner layers must never reach.
    pub(crate) nondeterminism_crates: BTreeSet<String>,
    /// Third-party crates each inner layer may depend on directly, by layer name.
    pub(crate) allowed_external: BTreeMap<String, BTreeSet<String>>,
    /// Crates allowed to contain `unsafe` code.
    pub(crate) unsafe_allowed: BTreeSet<String>,
}

/// A loaded workspace.
#[derive(Debug)]
pub(crate) struct Workspace {
    pub(crate) root: PathBuf,
    pub(crate) target_dir: PathBuf,
    pub(crate) policy: Policy,
    pub(crate) graph: Graph,
}

/// How to invoke `cargo metadata`.
#[derive(Debug, Default)]
pub(crate) struct LoadOptions {
    /// Manifest of the workspace to load; the current directory's when absent.
    pub(crate) manifest_path: Option<PathBuf>,
    /// Do not require an up-to-date `Cargo.lock`. Used for generated fixtures.
    pub(crate) unlocked: bool,
}

/// The `cargo` binary that started xtask, or the one on `PATH`.
pub(crate) fn cargo() -> Command {
    Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

/// Runs `cargo metadata` and builds the workspace model.
pub(crate) fn load(options: &LoadOptions) -> Result<Workspace> {
    let mut command = cargo();
    command.args(["metadata", "--format-version", "1", "--all-features"]);
    if options.unlocked {
        command.arg("--offline");
    } else {
        command.arg("--locked");
    }
    if let Some(path) = &options.manifest_path {
        command.arg("--manifest-path").arg(path);
    }
    let output = command.output().context("could not run `cargo metadata`")?;
    if !output.status.success() {
        return Err(Error::new(format!(
            "`cargo metadata` failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim_end()
        )));
    }
    let raw: RawMetadata =
        serde_json::from_slice(&output.stdout).context("could not parse `cargo metadata`")?;
    Ok(build(raw))
}

#[derive(Deserialize)]
struct RawMetadata {
    packages: Vec<RawPackage>,
    workspace_members: Vec<String>,
    resolve: Option<RawResolve>,
    workspace_root: PathBuf,
    target_directory: PathBuf,
    #[serde(default)]
    metadata: serde_json::Value,
}

#[derive(Deserialize)]
struct RawPackage {
    name: String,
    version: String,
    id: String,
    manifest_path: PathBuf,
    #[serde(default)]
    dependencies: Vec<RawDependency>,
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    metadata: serde_json::Value,
    publish: Option<Vec<String>>,
    #[serde(default)]
    targets: Vec<RawTarget>,
}

#[derive(Deserialize)]
struct RawDependency {
    name: String,
    kind: Option<String>,
    #[serde(default)]
    optional: bool,
    rename: Option<String>,
}

#[derive(Deserialize)]
struct RawTarget {
    kind: Vec<String>,
    src_path: PathBuf,
}

#[derive(Deserialize)]
struct RawResolve {
    nodes: Vec<RawNode>,
}

#[derive(Deserialize)]
struct RawNode {
    id: String,
    #[serde(default)]
    deps: Vec<RawNodeDep>,
}

#[derive(Deserialize)]
struct RawNodeDep {
    pkg: String,
    #[serde(default)]
    dep_kinds: Vec<RawDepKind>,
}

#[derive(Deserialize)]
struct RawDepKind {
    kind: Option<String>,
}

fn build(raw: RawMetadata) -> Workspace {
    let members: BTreeSet<&str> = raw.workspace_members.iter().map(String::as_str).collect();
    let mut edges: BTreeMap<String, Vec<Edge>> = BTreeMap::new();
    for node in raw.resolve.into_iter().flat_map(|resolve| resolve.nodes) {
        let node_edges = node
            .deps
            .into_iter()
            .map(|dep| {
                let mut kinds: Vec<DepKind> = dep
                    .dep_kinds
                    .iter()
                    .map(|kind| DepKind::parse(kind.kind.as_deref()))
                    .collect();
                kinds.sort_unstable();
                kinds.dedup();
                Edge { to: dep.pkg, kinds }
            })
            .collect();
        edges.insert(node.id, node_edges);
    }

    let mut graph = Graph::default();
    for package in raw.packages {
        let member = members
            .contains(package.id.as_str())
            .then(|| member(&package, &raw.workspace_root));
        graph.packages.insert(
            package.id.clone(),
            Package {
                name: package.name,
                version: package.version,
                member,
                edges: edges.remove(&package.id).unwrap_or_default(),
            },
        );
    }

    Workspace {
        policy: policy(&raw.metadata),
        root: raw.workspace_root,
        target_dir: raw.target_directory,
        graph,
    }
}

fn member(package: &RawPackage, root: &Path) -> Member {
    let dir = package
        .manifest_path
        .parent()
        .and_then(|dir| dir.strip_prefix(root).ok())
        .map_or_else(String::new, |dir| {
            dir.components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        });
    let waveport = package.metadata.get("waveport");
    let text = |key: &str| {
        waveport
            .and_then(|table| table.get(key))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let has_kind =
        |target: &RawTarget, kind: &str| target.kind.iter().any(|candidate| candidate == kind);
    let root_source = package
        .targets
        .iter()
        .find(|target| has_kind(target, "lib") || has_kind(target, "proc-macro"))
        .or_else(|| {
            package
                .targets
                .iter()
                .find(|target| has_kind(target, "bin"))
        })
        .map(|target| target.src_path.clone());
    Member {
        dir,
        layer: text("layer"),
        stability: text("stability"),
        has_build_script: package
            .targets
            .iter()
            .any(|target| has_kind(target, "custom-build")),
        publishable: package
            .publish
            .as_ref()
            .is_none_or(|registries| !registries.is_empty()),
        features: package.features.clone(),
        declared: package
            .dependencies
            .iter()
            .map(|dep| DeclaredDep {
                package: dep.name.clone(),
                feature_name: dep.rename.clone().unwrap_or_else(|| dep.name.clone()),
                kind: DepKind::parse(dep.kind.as_deref()),
                optional: dep.optional,
            })
            .collect(),
        root_source,
    }
}

fn policy(metadata: &serde_json::Value) -> Policy {
    let waveport = metadata.get("waveport");
    let arch = waveport.and_then(|table| table.get("arch"));
    let names = |value: Option<&serde_json::Value>| -> BTreeSet<String> {
        value
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_owned)
            .collect()
    };
    let allowed_external = arch
        .and_then(|table| table.get("allowed-external"))
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flatten()
        .map(|(layer, list)| (layer.clone(), names(Some(list))))
        .collect();
    Policy {
        runtime_crates: names(arch.and_then(|table| table.get("runtime-crates"))),
        nondeterminism_crates: names(arch.and_then(|table| table.get("nondeterminism-crates"))),
        allowed_external,
        unsafe_allowed: names(
            waveport
                .and_then(|table| table.get("unsafe"))
                .and_then(|table| table.get("allowed")),
        ),
    }
}

/// Builders for hand-made graphs in unit tests.
#[cfg(test)]
pub(crate) mod testing {
    use super::{DeclaredDep, DepKind, Edge, Graph, Member, Package};

    /// Builds a graph in which a package's identifier is its name.
    #[derive(Default)]
    pub(crate) struct GraphBuilder {
        graph: Graph,
    }

    impl GraphBuilder {
        pub(crate) fn member(mut self, name: &str, dir: &str, layer: &str) -> Self {
            let member = Member {
                dir: dir.to_owned(),
                layer: Some(layer.to_owned()),
                stability: Some("experimental".to_owned()),
                publishable: true,
                root_source: Some(format!("{dir}/src/lib.rs").into()),
                ..Member::default()
            };
            self.insert(name, Some(member));
            self
        }

        pub(crate) fn external(mut self, name: &str) -> Self {
            self.insert(name, None);
            self
        }

        pub(crate) fn edge(mut self, from: &str, to: &str, kind: DepKind) -> Self {
            let package = self.graph.packages.get_mut(from).unwrap();
            package.edges.push(Edge {
                to: to.to_owned(),
                kinds: vec![kind],
            });
            if let Some(member) = &mut package.member {
                member.declared.push(DeclaredDep {
                    package: to.to_owned(),
                    feature_name: to.to_owned(),
                    kind,
                    optional: false,
                });
            }
            self
        }

        pub(crate) fn edit(mut self, name: &str, change: impl FnOnce(&mut Member)) -> Self {
            change(
                self.graph
                    .packages
                    .get_mut(name)
                    .unwrap()
                    .member
                    .as_mut()
                    .unwrap(),
            );
            self
        }

        pub(crate) fn build(self) -> Graph {
            self.graph
        }

        fn insert(&mut self, name: &str, member: Option<Member>) {
            self.graph.packages.insert(
                name.to_owned(),
                Package {
                    name: name.to_owned(),
                    version: "1.0.0".to_owned(),
                    member,
                    edges: Vec::new(),
                },
            );
        }
    }
}
