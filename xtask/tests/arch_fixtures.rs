//! Runs `cargo xtask arch-check` on generated workspaces, one per rule.
//!
//! The unit tests in `arch.rs` check the rules on hand-made graphs. These
//! tests prove the other half: that the graph `cargo metadata` reports for a
//! real workspace is read correctly. Third-party crates are stand-ins under
//! an excluded `vendor/` directory, so nothing is downloaded.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers fail by panicking"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use Kind::{Build, Dev, Normal};

#[derive(Clone, Copy)]
enum Kind {
    Normal,
    Build,
    Dev,
}

impl Kind {
    fn table(self) -> &'static str {
        match self {
            Self::Normal => "dependencies",
            Self::Build => "build-dependencies",
            Self::Dev => "dev-dependencies",
        }
    }
}

struct Member {
    name: &'static str,
    dir: &'static str,
    layer: Option<&'static str>,
    deps: &'static [(&'static str, Kind)],
    build_script: bool,
}

/// A workspace generated on disk.
struct Fixture {
    name: &'static str,
    members: Vec<Member>,
    /// Stand-ins for third-party crates: name and dependencies.
    vendored: Vec<(&'static str, &'static [&'static str])>,
}

impl Fixture {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            members: Vec::new(),
            vendored: Vec::new(),
        }
    }

    fn member(
        mut self,
        name: &'static str,
        dir: &'static str,
        layer: &'static str,
        deps: &'static [(&'static str, Kind)],
    ) -> Self {
        self.members.push(Member {
            name,
            dir,
            layer: Some(layer),
            deps,
            build_script: false,
        });
        self
    }

    /// Changes the member added last.
    fn edit(mut self, change: impl FnOnce(&mut Member)) -> Self {
        change(self.members.last_mut().unwrap());
        self
    }

    fn vendored(mut self, name: &'static str, deps: &'static [&'static str]) -> Self {
        self.vendored.push((name, deps));
        self
    }

    /// Writes the workspace, runs the gate on it, and requires exactly the
    /// given rule codes. Returns the gate's output for further assertions.
    fn expect(self, codes: &[&str]) -> String {
        let root = self.write();
        let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(["arch-check", "--unlocked", "--manifest-path"])
            .arg(root.join("Cargo.toml"))
            // Violations in fixtures are expected; keep them out of CI annotations.
            .env_remove("GITHUB_ACTIONS")
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let found: BTreeSet<&str> = text
            .lines()
            .filter_map(|line| line.strip_prefix("error[")?.split(']').next())
            .collect();
        let expected: BTreeSet<&str> = codes.iter().copied().collect();
        assert_eq!(found, expected, "fixture `{}` printed:\n{text}", self.name);
        let exit = i32::from(!codes.is_empty());
        assert_eq!(
            output.status.code(),
            Some(exit),
            "fixture `{}` printed:\n{text}",
            self.name
        );
        text
    }

    fn write(&self) -> PathBuf {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("arch-fixtures")
            .join(self.name);
        // A stale copy from an earlier run may hold files this one does not write.
        let _ = std::fs::remove_dir_all(&root);

        let mut paths: BTreeMap<&str, PathBuf> = BTreeMap::new();
        for member in &self.members {
            paths.insert(member.name, root.join(member.dir));
        }
        for (name, _) in &self.vendored {
            paths.insert(name, root.join("vendor").join(name));
        }
        let dependency = |name: &str| {
            let path = paths
                .get(name)
                .unwrap_or_else(|| panic!("`{name}` is not in the fixture"));
            // A literal string, so Windows paths need no escaping.
            format!("{name} = {{ path = '{}' }}\n", path.display())
        };

        let members: Vec<String> = self
            .members
            .iter()
            .map(|member| format!("\"{}\"", member.dir))
            .collect();
        let manifest = format!(
            "[workspace]\n\
             resolver = \"3\"\n\
             members = [{}]\n\
             exclude = [\"vendor\"]\n\n\
             [workspace.metadata.waveport.arch]\n\
             runtime-crates = [\"tokio\"]\n\
             nondeterminism-crates = [\"getrandom\"]\n\n\
             [workspace.metadata.waveport.arch.allowed-external]\n\
             domain = [\"bytes\"]\n",
            members.join(", ")
        );
        write_file(&root.join("Cargo.toml"), &manifest);

        for member in &self.members {
            let mut manifest = package(member.name);
            manifest.push_str("\n[package.metadata.waveport]\nstability = \"experimental\"\n");
            if let Some(layer) = member.layer {
                writeln!(manifest, "layer = \"{layer}\"").unwrap();
            }
            for kind in [Normal, Build, Dev] {
                let table: String = member
                    .deps
                    .iter()
                    .filter(|(_, dep_kind)| dep_kind.table() == kind.table())
                    .map(|(name, _)| dependency(name))
                    .collect();
                if !table.is_empty() {
                    write!(manifest, "\n[{}]\n{table}", kind.table()).unwrap();
                }
            }
            let dir = &paths[member.name];
            write_file(&dir.join("Cargo.toml"), &manifest);
            write_file(&dir.join("src/lib.rs"), "");
            if member.build_script {
                write_file(&dir.join("build.rs"), "fn main() {}\n");
            }
        }

        for (name, deps) in &self.vendored {
            let mut manifest = package(name);
            // Outside the workspace, so the crate needs its own root.
            manifest.push_str("\n[workspace]\n\n[dependencies]\n");
            for dep in *deps {
                manifest.push_str(&dependency(dep));
            }
            let dir = &paths[name];
            write_file(&dir.join("Cargo.toml"), &manifest);
            write_file(&dir.join("src/lib.rs"), "");
        }
        root
    }
}

fn package(name: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n"
    )
}

fn write_file(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A workspace that follows every rule.
fn clean(name: &'static str) -> Fixture {
    Fixture::new(name)
        .member("dom", "crates/core/dom", "domain", &[("bytes", Normal)])
        .member("ports", "crates/core/ports", "ports", &[("dom", Normal)])
        .member("proto", "crates/core/proto", "protocol", &[])
        .member(
            "app",
            "crates/core/app",
            "application",
            &[("ports", Normal), ("proto", Normal), ("tokio", Normal)],
        )
        .member(
            "stt",
            "crates/adapters/stt/stt",
            "adapter",
            &[("ports", Normal), ("tokio", Normal)],
        )
        .member(
            "kit",
            "crates/testing/kit",
            "testing",
            &[("dom", Normal), ("tokio", Normal)],
        )
        .member(
            "daemon",
            "apps/daemon",
            "composition",
            &[("app", Normal), ("stt", Normal)],
        )
        .vendored("bytes", &[])
        .vendored("tokio", &[])
}

#[test]
fn ok_minimal() {
    clean("ok-minimal").expect(&[]);
}

#[test]
fn domain_direct_tokio() {
    Fixture::new("domain-direct-tokio")
        .member("dom", "crates/core/dom", "domain", &[("tokio", Normal)])
        .vendored("tokio", &[])
        .expect(&["ARCH002", "ARCH004"]);
}

#[test]
fn domain_transitive_tokio() {
    let output = Fixture::new("domain-transitive-tokio")
        .member("dom", "crates/core/dom", "domain", &[("bytes", Normal)])
        .vendored("bytes", &["helper"])
        .vendored("helper", &["tokio"])
        .vendored("tokio", &[])
        .expect(&["ARCH002"]);
    assert!(
        output.contains("dom -> bytes 0.0.0 -> helper 0.0.0 -> tokio 0.0.0"),
        "the dependency path is missing from:\n{output}"
    );
}

#[test]
fn domain_build_dep_tokio() {
    Fixture::new("domain-build-dep-tokio")
        .member("dom", "crates/core/dom", "domain", &[("tokio", Build)])
        .vendored("tokio", &[])
        .expect(&["ARCH002", "ARCH004"]);
}

#[test]
fn domain_dev_dep_tokio() {
    Fixture::new("domain-dev-dep-tokio")
        .member("dom", "crates/core/dom", "domain", &[("tokio", Dev)])
        .vendored("tokio", &[])
        .expect(&["ARCH006"]);
}

#[test]
fn domain_dev_dep_testkit() {
    // The test kit may use a runtime; depending on it in tests is allowed.
    Fixture::new("domain-dev-dep-testkit")
        .member("dom", "crates/core/dom", "domain", &[("kit", Dev)])
        .member("kit", "crates/testing/kit", "testing", &[("tokio", Normal)])
        .vendored("tokio", &[])
        .expect(&[]);
}

#[test]
fn domain_depends_adapter() {
    Fixture::new("domain-depends-adapter")
        .member("dom", "crates/core/dom", "domain", &[("stt", Normal)])
        .member("stt", "crates/adapters/stt/stt", "adapter", &[])
        .expect(&["ARCH001"]);
}

#[test]
fn adapter_depends_adapter() {
    Fixture::new("adapter-depends-adapter")
        .member(
            "stt",
            "crates/adapters/stt/stt",
            "adapter",
            &[("tts", Normal)],
        )
        .member("tts", "crates/adapters/tts/tts", "adapter", &[])
        .expect(&["ARCH003"]);
}

#[test]
fn adapter_transitive_adapter() {
    let output = Fixture::new("adapter-transitive-adapter")
        .member(
            "stt",
            "crates/adapters/stt/stt",
            "adapter",
            &[("daemon", Normal)],
        )
        .member("daemon", "apps/daemon", "composition", &[("tts", Normal)])
        .member("tts", "crates/adapters/tts/tts", "adapter", &[])
        .expect(&["ARCH001", "ARCH003"]);
    assert!(
        output.contains("stt -> daemon 0.0.0 -> tts 0.0.0"),
        "the dependency path is missing from:\n{output}"
    );
}

#[test]
fn external_not_allowlisted() {
    Fixture::new("external-not-allowlisted")
        .member("dom", "crates/core/dom", "domain", &[("helper", Normal)])
        .vendored("helper", &[])
        .expect(&["ARCH004"]);
}

#[test]
fn missing_metadata() {
    Fixture::new("missing-metadata")
        .member("dom", "crates/core/dom", "domain", &[])
        .edit(|member| member.layer = None)
        .expect(&["ARCH008"]);
}

#[test]
fn layer_path_mismatch() {
    Fixture::new("layer-path-mismatch")
        .member("stray", "crates/core/stray", "adapter", &[])
        .expect(&["ARCH008"]);
}

#[test]
fn domain_build_script() {
    Fixture::new("domain-build-script")
        .member("dom", "crates/core/dom", "domain", &[])
        .edit(|member| member.build_script = true)
        .expect(&["ARCH007"]);
}

#[test]
fn shipping_depends_on_testing() {
    Fixture::new("shipping-depends-on-testing")
        .member("app", "crates/core/app", "application", &[("kit", Normal)])
        .member("kit", "crates/testing/kit", "testing", &[])
        .expect(&["ARCH005"]);
}
