//! `cargo xtask fetch-assets`: downloads the files listed in the asset lock.
//!
//! Model weights and similar binaries are not committed. The lock file pins
//! each one to an immutable address and a SHA-256, and this command fetches
//! them into `target/assets/`. A file whose digest does not match is never
//! left in place.
//!
//! Two things make an entry more than a file at an address. An entry can
//! name the one platform it is for, and is then passed over on every
//! other: a shared library is such a file. And an entry can say that its
//! address serves an archive of which one member is the file: the archive
//! is checked against its own digest, the member is taken out with the
//! system's `tar`, checked against the entry's, and the archive is
//! discarded.

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
    /// The one platform the file is for, as [`platform`] writes it:
    /// `linux-x86_64`. Without it the file is for every platform.
    #[serde(default)]
    platform: Option<String>,
    /// Set when the address serves an archive that holds the file.
    #[serde(default)]
    archive: Option<Archive>,
}

/// The archive an asset is taken out of.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Archive {
    /// Lower-case hexadecimal SHA-256 of the archive.
    sha256: String,
    /// Size of the archive in bytes.
    size: u64,
    /// The path of the file inside the archive.
    member: String,
}

/// The platform this program runs on: `linux-x86_64`, `macos-aarch64`.
fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Whether an asset is for the platform `host`.
fn applies(asset: &Asset, host: &str) -> bool {
    asset
        .platform
        .as_deref()
        .is_none_or(|wanted| wanted == host)
}

fn is_digest(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Whether `member` is a plain path into an archive: names of letters,
/// digits, `.`, `-` and `_` with `/` between them, none of them `..`, and
/// nothing that `tar` could take for an option.
fn is_member(member: &str) -> bool {
    !member.is_empty()
        && member.split('/').all(|part| {
            !part.is_empty()
                && part != ".."
                && part != "."
                && !part.starts_with('-')
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        })
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
    if !is_digest(&asset.sha256) {
        return Some("`sha256` must be 64 lower-case hexadecimal digits".to_owned());
    }
    if asset.license.is_empty() || asset.source.is_empty() {
        return Some("`license` and `source` must not be empty".to_owned());
    }
    if let Some(archive) = &asset.archive {
        if !is_digest(&archive.sha256) {
            return Some("`archive.sha256` must be 64 lower-case hexadecimal digits".to_owned());
        }
        if !is_member(&archive.member) {
            return Some(
                "`archive.member` must be a plain path inside the archive: names of letters, \
                 digits, `.`, `-`, `_`, with `/` between them"
                    .to_owned(),
            );
        }
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
    holds(path, &asset.sha256, asset.size)
}

/// Whether `path` is a file with this digest and size.
fn holds(path: &Path, sha256: &str, size: u64) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let (found, length) = digest(path)?;
    Ok(found == sha256 && length == size)
}

/// The system's `tar`. On Windows the one that comes with the system is
/// named in full: it reads zip archives, and another `tar` earlier on the
/// path, such as the one a shell brings along, may not.
fn tar() -> std::path::PathBuf {
    if cfg!(windows)
        && let Some(root) = std::env::var_os("SystemRoot")
    {
        let system = Path::new(&root).join("System32").join("tar.exe");
        if system.is_file() {
            return system;
        }
    }
    "tar".into()
}

/// Takes `member` out of `archive` and leaves it at `to`. Returns whether
/// that worked.
fn extract(archive: &Path, member: &str, to: &Path) -> Result<bool> {
    let scratch = to.with_extension("unpack");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).context(format!("could not create {}", scratch.display()))?;
    let status = std::process::Command::new(tar())
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(&scratch)
        .arg(member)
        .stdin(Stdio::null())
        .status()
        .context("could not run `tar`; it is needed to unpack assets")?;
    let unpacked = scratch.join(member);
    let moved = status.success() && unpacked.is_file() && std::fs::rename(&unpacked, to).is_ok();
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(moved)
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

/// Checks the archive that was downloaded to `partial` and leaves there,
/// in its place, the one file wanted out of it. The archive is discarded
/// whatever happens.
fn unpack(
    asset: &Asset,
    archive: &Archive,
    dir: &Path,
    partial: &Path,
) -> Result<Option<Violation>> {
    let packed = dir.join(format!("{}.archive", asset.name));
    let _ = std::fs::remove_file(&packed);
    std::fs::rename(partial, &packed)
        .context(format!("could not move {} aside", partial.display()))?;
    let problem = if !holds(&packed, &archive.sha256, archive.size)? {
        let (sha256, size) = digest(&packed)?;
        Some(
            Violation::new(
                "ASSET001",
                format!(
                    "the archive of `{}` does not match the asset lock",
                    asset.name
                ),
                LOCK,
            )
            .note(
                "expected",
                format!("sha256 {} ({} bytes)", archive.sha256, archive.size),
            )
            .note("found", format!("sha256 {sha256} ({size} bytes)")),
        )
    } else if extract(&packed, &archive.member, partial)? {
        None
    } else {
        Some(
            Violation::new(
                "ASSET005",
                format!("could not take `{}` out of its archive", asset.name),
                LOCK,
            )
            .note("member", archive.member.clone()),
        )
    };
    let _ = std::fs::remove_file(&packed);
    Ok(problem)
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
    // An archive is checked as it came, and then stands aside for the one
    // file wanted out of it, which is checked like any other.
    let unpacked = match (&asset.archive, downloaded) {
        (Some(archive), true) => unpack(asset, archive, dir, &partial)?,
        _ => None,
    };
    let violation = if !downloaded {
        Some(
            Violation::new(
                "ASSET004",
                format!("could not download `{}`", asset.name),
                LOCK,
            )
            .note("url", asset.url.clone()),
        )
    } else if unpacked.is_some() {
        unpacked
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

    let host = platform();
    let mut violations = Vec::new();
    let mut wanted = 0;
    for asset in &lock.asset {
        // An entry for another platform is still held to the rules of the
        // lock, so that a mistake in it is found on every platform.
        if applies(asset, &host) {
            wanted += 1;
            violations.extend(ensure(asset, &dir, check_only)?);
        } else if let Some(reason) = invalid(asset) {
            violations.push(
                Violation::new(
                    "ASSET003",
                    format!("invalid entry `{}` in the asset lock", asset.name),
                    LOCK,
                )
                .note("reason", reason),
            );
        }
    }
    Ok(report::finish_with(
        "fetch-assets",
        &violations,
        wanted,
        "assets",
    ))
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::{
        Archive, Asset, Lock, applies, digest, extract, invalid, is_member, matches, platform, tar,
    };

    fn asset() -> Asset {
        Asset {
            name: "model-v1.onnx".to_owned(),
            url: "https://example.com/model.onnx".to_owned(),
            // SHA-256 of the three bytes `abc`.
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_owned(),
            size: 3,
            license: "MIT".to_owned(),
            source: "example".to_owned(),
            platform: None,
            archive: None,
        }
    }

    #[test]
    fn an_entry_for_one_platform_is_passed_over_on_the_others() {
        let host = platform();
        assert!(host.contains('-'), "{host}");
        assert!(applies(&asset(), &host));
        let here = Asset {
            platform: Some(host.clone()),
            ..asset()
        };
        assert!(applies(&here, &host));
        assert!(!applies(&here, "plan9-mips"));
    }

    #[test]
    fn a_member_of_an_archive_is_a_plain_path() {
        for member in ["lib/libx.so.1.2.3", "a", "dir-1/sub_2/file.dll"] {
            assert!(is_member(member), "{member:?} was refused");
        }
        for member in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "-xf",
            "a/-C",
            "a b",
            "a\\b",
            "./a",
        ] {
            assert!(!is_member(member), "{member:?} was accepted");
        }
        let entry = |member: &str, sha256: &str| Asset {
            archive: Some(Archive {
                sha256: sha256.to_owned(),
                size: 9,
                member: member.to_owned(),
            }),
            ..asset()
        };
        let digest = asset().sha256;
        assert_eq!(invalid(&entry("lib/libx.so", &digest)), None);
        assert!(invalid(&entry("../libx.so", &digest)).is_some());
        assert!(invalid(&entry("lib/libx.so", "abc")).is_some());
    }

    #[test]
    fn one_file_is_taken_out_of_an_archive() {
        let dir = std::env::temp_dir().join(format!("xtask-assets-unpack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pack/lib")).unwrap();
        std::fs::write(dir.join("pack/lib/libx.so.1"), "abc").unwrap();
        std::fs::write(dir.join("pack/readme"), "not wanted").unwrap();
        let archive = dir.join("pack.tar");
        let packed = Command::new(tar())
            .arg("-cf")
            .arg(&archive)
            .arg("-C")
            .arg(&dir)
            .arg("pack")
            .status()
            .unwrap();
        assert!(packed.success());

        let to = dir.join("libx.so.part");
        assert!(extract(&archive, "pack/lib/libx.so.1", &to).unwrap());
        assert!(matches(&asset(), &to).unwrap());
        // Nothing else is left behind.
        assert!(!dir.join("libx.so.unpack").exists());
        // A member that is not there.
        assert!(!extract(&archive, "pack/lib/missing", &dir.join("none.part")).unwrap());
        assert!(!dir.join("none.part").exists());
        std::fs::remove_dir_all(&dir).unwrap();
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
        // A platform and an archive are known keys, and what is in an
        // archive is held to the same rule.
        let packed = format!(
            "{entry}platform = \"linux-x86_64\"\n[asset.archive]\nsha256 = \"0\"\nsize = 2\n\
             member = \"lib/x.so\"\n"
        );
        let lock = toml::from_str::<Lock>(&packed).unwrap();
        assert_eq!(lock.asset[0].platform.as_deref(), Some("linux-x86_64"));
        assert_eq!(lock.asset[0].archive.as_ref().unwrap().member, "lib/x.so");
        assert!(toml::from_str::<Lock>(&format!("{packed}strip = 1\n")).is_err());
    }
}
