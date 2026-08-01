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
use crate::lock::{DerivedResult, EntryResult};

const FILE: &str = "fork-fold-state.json";

#[derive(Serialize, Deserialize)]
pub struct State {
    /// Index of the next manifest entry to process.
    pub next_index: usize,
    /// Base OID this build merges onto.
    pub base: String,
    /// Pins resolved for this run (lock pins plus any first-build pins).
    pub pins: BTreeMap<String, String>,
    /// Parent pins resolved for this run, entry name -> parent name -> OID.
    /// Carried here for the same reason `pins` is: a first build establishes
    /// them, and only a run that survives to `finalize` may write them to the
    /// lock.
    #[serde(default)]
    pub parent_pins: BTreeMap<String, BTreeMap<String, String>>,
    /// Anchors resolved for this run, entry name -> OID. A first build may
    /// have to detect one; every later build consumes what is already pinned.
    #[serde(default)]
    pub anchors: BTreeMap<String, String>,
    pub results: Vec<EntryResult>,
    pub conflicts: u32,
    /// Set when the run resumed an incremental extension.
    pub extended_from: Option<String>,
    /// The result of entry `next_index`'s merge, held back because its
    /// coherence fixup then failed to apply. Its presence is what tells
    /// `continue` the stall is in the fixup and the merge has ALREADY
    /// committed — re-running the merge would be wrong.
    #[serde(default)]
    pub pending: Option<EntryResult>,
    /// Set while a derived entry is being reconstructed. Its presence is what
    /// tells `continue` that the stall is in the DERIVE worktree, not the
    /// build one — the two are separate git worktrees with separate merges in
    /// flight, and resolving in the wrong one repairs nothing.
    #[serde(default)]
    pub derive: Option<DeriveState>,
    /// Reconstructions completed during this run, by entry name. Held apart
    /// from `derive` because a completed reconstruction still has to survive a
    /// stop in the stack merge that follows it: `continue` finishes that merge
    /// and must still record what was reconstructed.
    #[serde(default)]
    pub derived: BTreeMap<String, DerivedResult>,
}

/// The reconstruction of one derived entry, at whatever point it reached.
///
/// Persisted at every step, like the entry loop itself: reconstruction is a
/// merge sequence followed by a cherry-pick sequence, and resuming either one
/// at a stale index would re-merge or re-apply work that is already in.
#[derive(Serialize, Deserialize, Clone)]
pub struct DeriveState {
    /// Manifest position of the entry being reconstructed.
    pub entry_index: usize,
    /// Index into the entry's `parents` of the next one to merge.
    pub next_parent: usize,
    /// The merge of the base and every parent, once the last parent is in.
    /// Also the marker that `delta` has been computed: an empty delta is a
    /// legitimate answer (a pure merge of its parents adds nothing), so
    /// `delta.is_empty()` cannot mean "not worked out yet".
    pub base_tip: Option<String>,
    /// The entry's own commits, oldest first: everything after the anchor.
    pub delta: Vec<String>,
    /// Index into `delta` of the next commit to replay.
    pub next_pick: usize,
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
