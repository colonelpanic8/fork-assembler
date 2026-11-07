//! Thin wrappers around the real `git` binary.
//!
//! All git semantics come from shelling out (merge-ort behavior, credential
//! handling for free) — never libgit2/gitoxide.
//!
//! Two kinds of invocation. Transport (`transport`, `transport_out`) talks to
//! remotes and keeps the operator's configuration, because that is where
//! credentials, URL rewrites and SSH settings live. Everything else is build
//! git: it ignores global and system configuration, pins the settings that can
//! change a merge, a patch application or a commit object, and carries a fixed
//! identity, so a build depends on nothing about the machine it runs on.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};

/// The identity every generated commit carries, as author and committer.
/// Replayed commits keep their original author: git's cherry-pick takes it
/// from the picked commit, not from the environment.
pub const IDENTITY_NAME: &str = "fork-assembler";
pub const IDENTITY_EMAIL: &str = "fork-assembler@noreply.invalid";

/// Applied to every invocation: nothing machine-local may run or sign.
const SAFE_CFG: [&str; 3] = [
    "core.hooksPath=/dev/null",
    "commit.gpgSign=false",
    "merge.gpgSign=false",
];

/// Pinned for build git, on top of `SAFE_CFG`. Each one is a setting whose
/// value in a repository's own config would otherwise change a merge result,
/// an applied patch, a staged mode, or the bytes of a commit object. The
/// conflict style is not here: it is the manifest's to choose
/// (`rerere::merging`).
const BUILD_CFG: [&str; 13] = [
    "core.autocrlf=false",
    "core.fileMode=true",
    "core.commentChar=#",
    "apply.whitespace=nowarn",
    "commit.cleanup=default",
    "i18n.commitEncoding=UTF-8",
    "merge.renames=true",
    "merge.directoryRenames=conflict",
    "merge.renormalize=false",
    "merge.log=false",
    "merge.branchdesc=false",
    "merge.verifySignatures=false",
    "pull.twohead=ort",
];

/// Environment that would otherwise smuggle configuration or identity in.
const SCRUBBED_ENV: [&str; 8] = [
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_AUTHOR_DATE",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "GIT_COMMITTER_DATE",
];

fn with_cfg(cmd: &mut Command, cfg: &[&str]) {
    for setting in cfg {
        cmd.arg("-c").arg(setting);
    }
}

fn transport_command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    with_cfg(&mut cmd, &SAFE_CFG);
    cmd.args(args);
    cmd
}

fn command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    with_cfg(&mut cmd, &SAFE_CFG);
    with_cfg(&mut cmd, &BUILD_CFG);
    for var in SCRUBBED_ENV {
        cmd.env_remove(var);
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", IDENTITY_NAME)
        .env("GIT_AUTHOR_EMAIL", IDENTITY_EMAIL)
        .env("GIT_COMMITTER_NAME", IDENTITY_NAME)
        .env("GIT_COMMITTER_EMAIL", IDENTITY_EMAIL);
    cmd.args(args);
    cmd
}

fn exec(mut cmd: Command, args: &[&str]) -> Result<Output> {
    cmd.output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))
}

fn require(output: Output, dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
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

fn trimmed(stdout: Vec<u8>) -> String {
    String::from_utf8_lossy(&stdout).trim().to_string()
}

/// Run git, returning the raw Output without judging the exit status.
pub fn raw(dir: &Path, args: &[&str]) -> Result<Output> {
    exec(command(dir, args), args)
}

/// Run git, requiring success; returns stdout verbatim — no trimming, no
/// lossy UTF-8 conversion. For output that must survive byte-for-byte, such
/// as generated patches, where a stripped trailing newline breaks `git apply`.
pub fn bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    require(raw(dir, args)?, dir, args)
}

/// Run git, requiring success; returns trimmed stdout.
pub fn out(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(trimmed(bytes(dir, args)?))
}

/// True when the git command exits zero.
pub fn ok(dir: &Path, args: &[&str]) -> bool {
    raw(dir, args).map(|o| o.status.success()).unwrap_or(false)
}

/// Run a remote-facing command (fetch, push, ls-remote, clone) with the
/// operator's configuration intact.
pub fn transport(dir: &Path, args: &[&str]) -> Result<Output> {
    exec(transport_command(dir, args), args)
}

/// `transport`, requiring success; returns trimmed stdout.
pub fn transport_out(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(trimmed(require(transport(dir, args)?, dir, args)?))
}

/// True when `oid` resolves to a commit present in `dir`'s object store.
pub fn has_commit(dir: &Path, oid: &str) -> bool {
    ok(dir, &["cat-file", "-e", &format!("{oid}^{{commit}}")])
}

/// True when `commit` is reachable from `of`.
pub fn is_ancestor(dir: &Path, commit: &str, of: &str) -> bool {
    ok(dir, &["merge-base", "--is-ancestor", commit, of])
}

/// The worktree's own git dir, absolute — where an in-flight merge or
/// cherry-pick records itself, and where this tool keeps its build state.
pub fn git_dir(worktree: &Path) -> Result<PathBuf> {
    let dir = PathBuf::from(out(worktree, &["rev-parse", "--git-dir"])?);
    Ok(if dir.is_absolute() {
        dir
    } else {
        worktree.join(dir)
    })
}

/// The abbreviated form every report prints an OID in.
pub fn short(oid: &str) -> &str {
    &oid[..12.min(oid.len())]
}
