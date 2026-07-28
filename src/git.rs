//! Thin wrappers around the real `git` binary.
//!
//! All git semantics come from shelling out (merge-ort behavior, config and
//! credential handling for free) — never libgit2/gitoxide. Every invocation
//! disables hooks and signing so generated commits are reproducible and never
//! blocked by machine-local configuration.

use std::path::Path;
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};

const SAFE_CFG: [&str; 6] = [
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "commit.gpgSign=false",
    "-c",
    "merge.gpgSign=false",
];

fn command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    cmd.args(SAFE_CFG);
    cmd.args(args);
    cmd
}

/// Run git, returning the raw Output without judging the exit status.
pub fn raw(dir: &Path, args: &[&str]) -> Result<Output> {
    command(dir, args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))
}

/// Run git, requiring success; returns trimmed stdout.
pub fn out(dir: &Path, args: &[&str]) -> Result<String> {
    let output = raw(dir, args)?;
    if !output.status.success() {
        bail!(
            "git {} failed in {}:\n{}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Run git, requiring success; returns raw stdout bytes (binary-safe).
pub fn out_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = raw(dir, args)?;
    if !output.status.success() {
        bail!(
            "git {} failed in {}:\n{}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// True when the git command exits zero.
pub fn ok(dir: &Path, args: &[&str]) -> bool {
    raw(dir, args).map(|o| o.status.success()).unwrap_or(false)
}

/// True when `oid` resolves to a commit present in `dir`'s object store.
pub fn has_commit(dir: &Path, oid: &str) -> bool {
    ok(dir, &["cat-file", "-e", &format!("{oid}^{{commit}}")])
}
