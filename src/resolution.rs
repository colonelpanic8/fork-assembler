//! Tracked conflict-resolution sidecars.
//!
//! One pair per conflicted entry, keyed by (sanitized) entry name:
//!
//! ```text
//! resolutions/<name>.toml    # exact-match key: ours_tree, theirs, resolved_tree
//! resolutions/<name>.patch   # binary-safe full diff: first-parent tree -> resolved tree
//! ```
//!
//! The patch is a FULL tree diff, so replay restores the first-parent state
//! and applies it wholesale — never contextual conflict-marker matching.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::git;
use crate::manifest::sanitize_name;

pub const DIR: &str = "resolutions";

#[derive(Serialize, Deserialize)]
pub struct Record {
    /// First-parent tree the conflicted merge started from.
    pub ours_tree: String,
    /// The topic commit being merged (MERGE_HEAD).
    pub theirs: String,
    /// Tree of the resolved merge.
    pub resolved_tree: String,
}

fn toml_path(root: &Path, name: &str) -> PathBuf {
    root.join(DIR).join(format!("{}.toml", sanitize_name(name)))
}

pub fn patch_path(root: &Path, name: &str) -> PathBuf {
    root.join(DIR).join(format!("{}.patch", sanitize_name(name)))
}

/// Repo-relative patch path, for lock/result reporting.
pub fn patch_rel(name: &str) -> String {
    format!("{DIR}/{}.patch", sanitize_name(name))
}

pub fn load(root: &Path, name: &str) -> Result<Option<Record>> {
    let path = toml_path(root, name);
    if !path.exists() {
        return Ok(None);
    }
    let record: Record = toml::from_str(&fs::read_to_string(&path)?)
        .with_context(|| format!("parsing {}", path.display()))?;
    if !patch_path(root, name).exists() {
        bail!(
            "{} exists but its patch {} is missing",
            path.display(),
            patch_rel(name)
        );
    }
    Ok(Some(record))
}

/// Write/rewrite the sidecar pair from a committed merge in `worktree` whose
/// HEAD is the resolved merge commit.
pub fn record_from_merge_head(root: &Path, worktree: &Path, name: &str) -> Result<Record> {
    let ours_tree = git::out(worktree, &["rev-parse", "HEAD^1^{tree}"])?;
    let theirs = git::out(worktree, &["rev-parse", "HEAD^2"])?;
    let resolved_tree = git::out(worktree, &["rev-parse", "HEAD^{tree}"])?;
    let patch = git::out_bytes(
        worktree,
        &[
            "diff-tree",
            "--binary",
            "--full-index",
            "-p",
            &ours_tree,
            &resolved_tree,
        ],
    )?;

    fs::create_dir_all(root.join(DIR))?;
    let record = Record {
        ours_tree,
        theirs,
        resolved_tree,
    };
    let mut body = String::from(
        "# fork-fold resolution record: full first-parent -> resolved-merge diff.\n\
         # Replayed only when ours_tree and theirs both match exactly; otherwise\n\
         # proposed via 3-way merge for human confirmation.\n",
    );
    body.push_str(&toml::to_string(&record)?);
    fs::write(toml_path(root, name), body)?;
    fs::write(patch_path(root, name), patch)?;
    Ok(record)
}

pub enum Replay {
    /// Inputs matched exactly; the resolved merge is committed.
    Exact,
    /// Record exists but inputs drifted; the old resolution is staged as a
    /// 3-way proposal for human review (worktree left mid-merge).
    Proposed,
    /// No record for this entry; the conflict is untouched.
    None,
}

/// Try to resolve the in-progress conflicted merge of `name` in `worktree`.
pub fn replay(root: &Path, worktree: &Path, name: &str, topic_oid: &str) -> Result<Replay> {
    let Some(record) = load(root, name)? else {
        return Ok(Replay::None);
    };
    let ours_tree = git::out(worktree, &["rev-parse", "HEAD^{tree}"])?;
    let merge_head = git::out(worktree, &["rev-parse", "MERGE_HEAD"])?;
    let patch = patch_path(root, name);
    let patch_str = patch.to_string_lossy().to_string();

    // Restore the first-parent state without aborting the merge, then apply
    // the recorded full diff.
    git::out(
        worktree,
        &["restore", "--source=HEAD", "--staged", "--worktree", "--", "."],
    )?;

    let exact =
        ours_tree == record.ours_tree && merge_head == record.theirs && merge_head == topic_oid;
    if exact {
        let applied = git::raw(worktree, &["apply", "--binary", "--index", &patch_str])?;
        if !applied.status.success() {
            bail!(
                "{name}: recorded resolution failed to apply:\n{}",
                String::from_utf8_lossy(&applied.stderr).trim()
            );
        }
        let resolved = git::out(worktree, &["write-tree"])?;
        if resolved != record.resolved_tree {
            bail!(
                "{name}: resolution produced tree {resolved}, expected {}",
                record.resolved_tree
            );
        }
        git::out(worktree, &["commit", "--no-edit"])?;
        Ok(Replay::Exact)
    } else {
        // Stale record: propose it. --3way needs the pre-image blobs, which
        // --full-index diffs reference; leftovers stage as conflicts.
        let _ = git::raw(worktree, &["apply", "--3way", "--binary", &patch_str])?;
        Ok(Replay::Proposed)
    }
}
