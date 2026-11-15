//! The few git queries the gates need.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::{Context, Error, Result};

/// Whether `path` exists in the tree of revision `rev`.
pub(crate) fn exists_at(root: &Path, rev: &str, path: &str) -> Result<bool> {
    let status = Command::new("git")
        .current_dir(root)
        .args(["cat-file", "-e", &format!("{rev}:{path}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("could not run `git cat-file`")?;
    Ok(status.success())
}

/// The commit `HEAD` and `branch` have in common, when both exist.
pub(crate) fn merge_base(root: &Path, branch: &str) -> Option<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["merge-base", "HEAD", branch])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let rev = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (output.status.success() && !rev.is_empty()).then_some(rev)
}

/// Runs a git command that must succeed.
pub(crate) fn run(root: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .current_dir(root)
        .args(args)
        .status()
        .context("could not run `git`")?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::new(format!("`git {}` failed", args.join(" "))))
    }
}
