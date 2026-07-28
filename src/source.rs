//! The source repository: where git objects live and builds run.
//!
//! If `[base].submodule` is set, that checkout is the source. Otherwise the
//! base remote is auto-cloned into `.worktrees/source` on first use. Either
//! way, every `[remotes]` name is materialized as a real git remote and
//! rerere is disabled in the repo config: builds enable it per-command and
//! seed its cache exclusively from tracked pairs (see `rerere.rs`), so
//! nothing ambient is ever recorded or consulted.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::git;
use crate::manifest::Manifest;

pub fn source_path(root: &Path, manifest: &Manifest) -> PathBuf {
    match &manifest.base.submodule {
        Some(sub) => root.join(sub),
        None => root.join(".worktrees").join("source"),
    }
}

fn configure(path: &Path, manifest: &Manifest) -> Result<()> {
    for (name, url) in &manifest.remotes {
        ensure_remote(path, name, url)?;
    }
    git::out(path, &["config", "rerere.enabled", "false"])?;
    Ok(())
}

fn ensure_remote(repo: &Path, name: &str, url: &str) -> Result<()> {
    let current = git::raw(repo, &["remote", "get-url", name])?;
    if current.status.success() {
        let existing = String::from_utf8_lossy(&current.stdout).trim().to_string();
        if existing != url {
            git::out(repo, &["remote", "set-url", name, url])?;
        }
    } else {
        git::out(repo, &["remote", "add", name, url])?;
    }
    Ok(())
}

/// Locate (and, when permitted, create) the source repo, then configure it.
pub fn source_repo(root: &Path, manifest: &Manifest, allow_clone: bool) -> Result<PathBuf> {
    let path = source_path(root, manifest);
    if manifest.base.submodule.is_some() {
        if !git::ok(&path, &["rev-parse", "--git-dir"]) {
            bail!(
                "{} is not an initialized git checkout; run `git submodule update --init`",
                path.display()
            );
        }
    } else if !path.exists() {
        if !allow_clone {
            bail!(
                "no source repository at {} (refusing to clone; drop --locked or run a build/update first)",
                path.display()
            );
        }
        let url = manifest.remote_url(&manifest.base.remote)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        println!("cloning {} into {}...", url, path.display());
        git::out(root, &["clone", url, &path.to_string_lossy()])?;
    } else if !git::ok(&path, &["rev-parse", "--git-dir"]) {
        bail!(
            "{} exists but is not a git repository; remove it and retry",
            path.display()
        );
    }
    configure(&path, manifest)?;
    Ok(path)
}

/// The source repo if it already exists, configured; None otherwise.
/// For offline verbs (status) that must degrade gracefully.
pub fn source_repo_if_present(root: &Path, manifest: &Manifest) -> Option<PathBuf> {
    let path = source_path(root, manifest);
    if git::ok(&path, &["rev-parse", "--git-dir"]) {
        // Best-effort: skip configuration errors in read-only contexts.
        let _ = configure(&path, manifest);
        Some(path)
    } else {
        None
    }
}
