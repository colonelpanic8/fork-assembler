//! In-progress build state, stored in the build worktree's git-dir.
//!
//! Persisted after EVERY entry, not just on conflict: a crash mid-run must
//! never resume at a stale index, re-merge an already-merged entry, and
//! falsely report it EMPTY — that would corrupt the drop-candidate signal.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::git;
use crate::lock::EntryResult;

const FILE: &str = "fork-fold-state.json";

#[derive(Serialize, Deserialize)]
pub struct State {
    /// Index of the next manifest entry to process.
    pub next_index: usize,
    /// Base OID this build merges onto.
    pub base: String,
    /// Pins resolved for this run (lock pins plus any first-build pins).
    pub pins: BTreeMap<String, String>,
    pub results: Vec<EntryResult>,
    pub conflicts: u32,
    /// Set when the run resumed an incremental extension.
    pub extended_from: Option<String>,
}

fn state_path(worktree: &Path) -> Result<PathBuf> {
    let git_dir = git::out(worktree, &["rev-parse", "--git-dir"])?;
    let mut dir = PathBuf::from(&git_dir);
    if !dir.is_absolute() {
        dir = worktree.join(dir);
    }
    Ok(dir.join(FILE))
}

pub fn read(worktree: &Path) -> Result<Option<State>> {
    let path = state_path(worktree)?;
    if !path.exists() {
        return Ok(None);
    }
    let state = serde_json::from_str(&fs::read_to_string(&path)?)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(state))
}

pub fn write(worktree: &Path, state: &State) -> Result<()> {
    let path = state_path(worktree)?;
    fs::write(&path, serde_json::to_string_pretty(state)?)
        .with_context(|| format!("writing {}", path.display()))
}

pub fn clear(worktree: &Path) -> Result<()> {
    let path = state_path(worktree)?;
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}
