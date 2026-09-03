//! What a build has to say, separated from how it is said.
//!
//! The engine emits `Event`s and never writes to stdout itself. Every event
//! is owned, serializable data: the text renderer and the JSON renderer in
//! `output` receive the same values, and so can anything else that wants to
//! watch a build.

use std::path::PathBuf;

use serde::Serialize;

use crate::engine::derive::AnchorRule;
use crate::engine::Step;

pub trait Report {
    fn event(&self, event: &Event);
}

/// Which entry an event is about, and where it sits in the run.
#[derive(Serialize, Clone)]
pub struct StepInfo {
    pub index: usize,
    pub total: usize,
    pub entry: String,
}

impl Step<'_> {
    pub fn info(&self) -> StepInfo {
        StepInfo {
            index: self.index,
            total: self.total,
            entry: self.entry.name.clone(),
        }
    }
}

/// How applying a patch file ended, when it did not fail.
#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Applied,
    AlreadyApplied,
}

/// How replaying one of a derived entry's own commits ended.
#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum Replay {
    Clean,
    /// Its content was already in the reconstruction; nothing to commit.
    AlreadyPresent,
    /// Conflicted, and tracked rerere pairs resolved every hunk.
    AutoResolved,
}

/// What `continue` just committed on the operator's behalf.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resolved {
    /// The entry's merge into the stack.
    StackMerge,
    /// One parent's merge inside a reconstruction.
    ParentMerge { parent: String },
    /// One replayed commit inside a reconstruction.
    Replay { commit: String },
}

/// What a reconstruction was in the middle of when it stopped.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Doing {
    MergingParent { parent: String },
    Replaying { commit: String, subject: String },
}

#[derive(Serialize, Clone)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The source repository is being cloned on first use.
    Cloning {
        url: String,
        path: PathBuf,
    },

    // Pinning, before the run starts.
    PinnedBase {
        oid: String,
    },
    Pinned {
        entry: String,
        oid: String,
    },
    PinnedParent {
        entry: String,
        parent: String,
        oid: String,
    },

    // How the run starts.
    UpToDate {
        tree: String,
    },
    Extending {
        new_entries: usize,
    },
    FromBase {
        base: String,
    },
    Seeded {
        pairs: usize,
    },
    Resuming {
        index: usize,
        total: usize,
    },

    // One entry's step.
    Applied {
        #[serde(flatten)]
        step: StepInfo,
        outcome: Outcome,
    },
    PatchFailed {
        #[serde(flatten)]
        step: StepInfo,
        stderr: String,
        worktree: PathBuf,
    },
    Fixup {
        #[serde(flatten)]
        step: StepInfo,
        path: String,
        outcome: Outcome,
    },
    FixupFailed {
        #[serde(flatten)]
        step: StepInfo,
        path: String,
        stderr: String,
        worktree: PathBuf,
    },
    /// `continue` committed a hand-resolved fixup, whose patch file is now
    /// stale.
    FixupCommitted {
        #[serde(flatten)]
        step: StepInfo,
        path: String,
    },
    Absorbed {
        #[serde(flatten)]
        step: StepInfo,
    },
    Empty {
        #[serde(flatten)]
        step: StepInfo,
    },
    Merged {
        #[serde(flatten)]
        step: StepInfo,
        oid: String,
        reconstruction: bool,
    },
    Conflict {
        #[serde(flatten)]
        step: StepInfo,
        files: Vec<String>,
        worktree: PathBuf,
    },
    AutoResolved {
        #[serde(flatten)]
        step: StepInfo,
    },
    Harvested {
        #[serde(flatten)]
        step: StepInfo,
        resolved: Resolved,
        pairs: Vec<String>,
    },

    // Reconstructing a derived entry.
    Reconstructing {
        #[serde(flatten)]
        step: StepInfo,
        base: String,
        worktree: PathBuf,
    },
    DeriveConflict {
        #[serde(flatten)]
        step: StepInfo,
        doing: Doing,
        files: Vec<String>,
        worktree: PathBuf,
    },
    ParentAbsorbed {
        #[serde(flatten)]
        step: StepInfo,
        parent: String,
    },
    ParentMerged {
        #[serde(flatten)]
        step: StepInfo,
        parent: String,
        oid: String,
    },
    ParentAutoResolved {
        #[serde(flatten)]
        step: StepInfo,
        parent: String,
    },
    Anchor {
        #[serde(flatten)]
        step: StepInfo,
        oid: String,
        rule: AnchorRule,
    },
    Delta {
        #[serde(flatten)]
        step: StepInfo,
        commits: usize,
    },
    Replayed {
        #[serde(flatten)]
        step: StepInfo,
        commit: String,
        subject: String,
        outcome: Replay,
    },
    /// `continue` found the resolved pick empty and skipped it.
    ReplaySkipped {
        #[serde(flatten)]
        step: StepInfo,
        commit: String,
    },
    PublishSkipped {
        entry: String,
        target: String,
    },
    Published {
        entry: String,
        oid: String,
        target: String,
    },

    // The end of a completed run.
    Finished {
        tree: String,
        commit: String,
        conflicts: u32,
        previous_tree: Option<String>,
    },
    LockWritten,
    Verified,
    NothingToVerify,
}
