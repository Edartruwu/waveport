//! `cargo xtask fetch-assets`: downloads the files listed in the asset lock.
//!
//! Model weights and similar binaries are not committed. The lock file pins
//! each one to an immutable address and a SHA-256, and this command fetches
//! them into `target/assets/`. A file whose digest does not match is never
//! left in place.

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::Path;
use std::process::Stdio;

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::args::Args;
use crate::error::{Context, Result};
use crate::meta::{self, LoadOptions};
use crate::report::{self, Violation};

/// The lock file, relative to the workspace root.
const LOCK: &str = "assets/assets.lock.toml";

/// Where the files are stored, relative to the target directory.
const DIR: &str = "assets";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Lock {
    #[serde(default)]
    asset: Vec<Asset>,
}

/// One pinned file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Asset {
    /// File name under the assets directory.
    name: String,
    /// Where to download it from. Must not change what it serves.
    url: String,
    /// Lower-case hexadecimal SHA-256 of the file.
    sha256: String,
    /// Size in bytes.
    size: u64,
    /// SPDX identifier of the licence the file is distributed under.
    license: String,
    /// Where the file comes from, for a human reader.
    source: String,
}

/// What is wrong with a lock entry, if anything.
fn invalid(asset: &Asset) -> Option<String> {
    let plain_name = !asset.name.is_empty()
        && asset
            .name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        && !asset.name.starts_with('.');
    if !plain_name {
        return Some("`name` must be a plain file name: letters, digits, `.`, `-`, `_`".to_owned());
    }
    if !asset.url.starts_with("https://") || asset.url.contains(char::is_whitespace) {
        return Some("`url` must be an https address".to_owned());
    }
    let hex = asset.sha256.len() == 64
        && asset
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !hex {
        return Some("`sha256` must be 64 lower-case hexadecimal digits".to_owned());
    }
    if asset.license.is_empty() || asset.source.is_empty() {
        return Some("`license` and `source` must not be empty".to_owned());
    }
    None
}

/// The SHA-256 and size of a file.
fn digest(path: &Path) -> Result<(String, u64)> {
    let mut file =
        std::fs::File::open(path).context(format!("could not open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut size = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .context(format!("could not read {}", path.display()))?;
        let Some(chunk) = buffer.get(..read).filter(|chunk| !chunk.is_empty()) else {
            break;
        };
        hasher.update(chunk);
        size = size.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    }
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        // Writing to a `String` cannot fail.
        let _ = write!(hex, "{byte:02x}");
    }
    Ok((hex, size))
}

/// Whether `path` holds exactly the file the lock describes.
fn matches(asset: &Asset, path: &Path) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let (sha256, size) = digest(path)?;
    Ok(sha256 == asset.sha256 && size == asset.size)
}

fn download(asset: &Asset, to: &Path) -> Result<bool> {
    let status = std::process::Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--max-time",
            "600",
            "--output",
        ])
        .arg(to)
        .arg(&asset.url)
        .stdin(Stdio::null())
        .status()
        .context("could not run `curl`; it is needed to download assets")?;
    Ok(status.success())
}

/// Makes sure one asset is present and intact, downloading it unless `check_only`.
fn ensure(asset: &Asset, dir: &Path, check_only: bool) -> Result<Option<Violation>> {
    if let Some(reason) = invalid(asset) {
        return Ok(Some(
            Violation::new(
                "ASSET003",
                format!("invalid entry `{}` in the asset lock", asset.name),
                LOCK,
            )
            .note("reason", reason),
        ));
    }
    let path = dir.join(&asset.name);
    if matches(asset, &path)? {
        println!("    {}: ok", asset.name);
        return Ok(None);
    }
    if check_only {
        return Ok(Some(
            Violation::new(
                "ASSET002",
                format!(
                    "`{}` is missing or does not match the asset lock",
                    asset.name
                ),
                LOCK,
            )
            .note("help", "run `cargo xtask fetch-assets`"),
        ));
    }

    println!("    {}: downloading from {}", asset.name, asset.source);
    let partial = dir.join(format!("{}.part", asset.name));
    let downloaded = download(asset, &partial)?;
    let violation = if !downloaded {
        Some(
            Violation::new(
                "ASSET004",
                format!("could not download `{}`", asset.name),
                LOCK,
            )
            .note("url", asset.url.clone()),
        )
    } else if matches(asset, &partial)? {
        std::fs::rename(&partial, &path)
            .context(format!("could not move {} into place", partial.display()))?;
        None
    } else {
        let (sha256, size) = digest(&partial)?;
        Some(
            Violation::new(
                "ASSET001",
                format!("`{}` does not match the asset lock", asset.name),
                LOCK,
            )
            .note(
                "expected",
                format!("sha256 {} ({} bytes)", asset.sha256, asset.size),
            )
            .note("found", format!("sha256 {sha256} ({size} bytes)"))
            .note(
                "rule",
                "a pinned address must keep serving the same file; a file that differs is discarded",
            ),
        )
    };
    if violation.is_some() {
        // Never leave an unverified file where a later run could mistake it.
        let _ = std::fs::remove_file(&partial);
    }
    Ok(violation)
}

/// Fetches every asset, or with `--check` only verifies what is present.
pub(crate) fn run(args: &[String]) -> Result<bool> {
    let mut args = Args::new(args);
    let check_only = args.flag("--check");
    args.finish()?;
    let workspace = meta::load(&LoadOptions::default())?;
    let lock_path = workspace.root.join(LOCK);
    let text = std::fs::read_to_string(&lock_path)
        .context(format!("could not read {}", lock_path.display()))?;
    let lock: Lock = toml::from_str(&text).context(format!("could not parse {LOCK}"))?;
    let dir = workspace.target_dir.join(DIR);
    std::fs::create_dir_all(&dir).context(format!("could not create {}", dir.display()))?;

    let mut violations = Vec::new();
    for asset in &lock.asset {
        violations.extend(ensure(asset, &dir, check_only)?);
    }
    Ok(report::finish_with(
        "fetch-assets",
        &violations,
        lock.asset.len(),
        "assets",
    ))
}

#[cfg(test)]
mod tests {
    use super::{Asset, Lock, digest, invalid, matches};

    fn asset() -> Asset {
        Asset {
            name: "model-v1.onnx".to_owned(),
            url: "https://example.com/model.onnx".to_owned(),
            // SHA-256 of the three bytes `abc`.
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_owned(),
            size: 3,
            license: "MIT".to_owned(),
            source: "example".to_owned(),
        }
    }

    #[test]
    fn a_well_formed_entry_is_valid() {
        assert_eq!(invalid(&asset()), None);
    }

    #[test]
    fn names_cannot_leave_the_assets_directory() {
        for name in ["", "../model", "dir/model", ".hidden", "a b", "c:\\model"] {
            let entry = Asset {
                name: name.to_owned(),
                ..asset()
            };
            assert!(invalid(&entry).is_some(), "{name:?} was accepted");
        }
    }

    #[test]
    fn only_https_addresses_and_full_digests_are_accepted() {
        for url in [
            "http://example.com/m",
            "file:///etc/passwd",
            "-o /tmp/x",
            "https://a b",
        ] {
            let entry = Asset {
                url: url.to_owned(),
                ..asset()
            };
            assert!(invalid(&entry).is_some(), "{url:?} was accepted");
        }
        for sha256 in ["", "abc", &"A".repeat(64), &"g".repeat(64), &"a".repeat(65)] {
            let entry = Asset {
                sha256: sha256.to_owned(),
                ..asset()
            };
            assert!(invalid(&entry).is_some(), "{sha256:?} was accepted");
        }
    }

    #[test]
    fn the_digest_is_sha_256() {
        let dir = std::env::temp_dir().join(format!("xtask-assets-digest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("abc");
        std::fs::write(&path, "abc").unwrap();
        assert_eq!(digest(&path).unwrap(), (asset().sha256, 3));
        assert!(matches(&asset(), &path).unwrap());

        std::fs::write(&path, "abd").unwrap();
        assert!(!matches(&asset(), &path).unwrap());
        assert!(!matches(&asset(), &dir.join("missing")).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_lock_rejects_unknown_keys() {
        let entry = "[[asset]]\nname = \"a\"\nurl = \"https://e/x\"\nsha256 = \"0\"\nsize = 1\n\
                     license = \"MIT\"\nsource = \"s\"\n";
        assert_eq!(toml::from_str::<Lock>(entry).unwrap().asset.len(), 1);
        assert!(toml::from_str::<Lock>(&format!("{entry}checksum = \"1\"\n")).is_err());
        assert_eq!(toml::from_str::<Lock>("").unwrap().asset.len(), 0);
    }
}
