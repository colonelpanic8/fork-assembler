//! `manifest.lock.json` — FACT: the OIDs the last build used and its result.
//!
//! `pins` records which OID each ref was resolved to. Pins move only via
//! `update` (or on an entry's very first build); `build` consumes them.
//! `build` records what the last completed build produced. `tree` (the content
//! tree BEFORE the provenance commit) is the reproducibility invariant;
//! `built_tree` is what ships.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::git;
use crate::manifest::Entry;

pub const FILE: &str = "manifest.lock.json";

#[derive(Serialize, Deserialize, Clone)]
pub struct Lock {
    pub version: u32,
    #[serde(default)]
    pub pins: Pins,
    #[serde(default)]
    pub build: Option<BuildRecord>,
}

impl Default for Lock {
    fn default() -> Self {
        Lock {
            version: 1,
            pins: Pins::default(),
            build: None,
        }
    }
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Pins {
    pub base: Option<String>,
    #[serde(default)]
    pub entries: BTreeMap<String, String>,
    /// Derived entries only: entry name -> parent name -> OID. A parent is a
    /// tracked ref like any other, so its pin obeys the same rule the entry
    /// pins do — `update` and an entry's first build set it, `build` consumes
    /// it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parents: BTreeMap<String, BTreeMap<String, String>>,
    /// Derived entries only: entry name -> the commit in the entry's history
    /// after which its own commits start. Established whenever the entry's pin
    /// is, and moved by nothing else: the delta is defined relative to it, so
    /// a `build` that re-derived it could silently change what it replays.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub anchors: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct BuildRecord {
    pub generated: String,
    /// Base OID this build merged onto.
    pub base: String,
    /// Assembled head (after the provenance commit, when one is written).
    pub commit: String,
    /// Last commit before the provenance commit; incremental builds extend
    /// from here. Equal to `commit` when no provenance file is configured.
    pub pre_provenance_commit: String,
    /// Content tree BEFORE the provenance commit — the reproducibility
    /// invariant. Compare this, not `built_tree`, to decide whether anything
    /// actually moved.
    pub tree: String,
    /// Tree of `commit` (i.e. `tree` plus the provenance record).
    pub built_tree: String,
    pub previous_tree: Option<String>,
    pub tree_changed: bool,
    /// Conflicts encountered during this build run (including replayed
    /// resolutions).
    pub conflicts: u32,
    /// Snapshot of the manifest entries this build processed, sufficient to
    /// detect prefix-extension.
    pub manifest_entries: Vec<SnapshotEntry>,
    /// One result per manifest entry, in order.
    pub results: Vec<EntryResult>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct SnapshotEntry {
    pub name: String,
    pub kind: String,
    pub source: String,
    pub pin: String,
    /// Blob hash of the entry's coherence fixup, when it carries one. Part of
    /// the snapshot so editing a fixup invalidates the suffix from its entry
    /// and the next `build` redoes exactly that much — no `update` needed,
    /// since a fixup is repo-local content, not a tracked remote ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixup: Option<String>,
    /// A requested remote branch publication is part of the snapshot so adding
    /// or changing it reruns the entry instead of treating the build as up to
    /// date and silently skipping the requested push.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconstruction_publish: Option<String>,
    /// A derived entry's parent pins, in manifest order. In the snapshot for
    /// the same reason the fixup blob is: a parent that moved changes what
    /// this entry's step produces, so it must invalidate the suffix from here
    /// exactly as repinning the entry would.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parents: Vec<ParentPin>,
    /// A derived entry's anchor: move it and the replayed delta changes, which
    /// is another way of changing what this step produces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ParentPin {
    pub source: String,
    pub pin: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct EntryResult {
    pub name: String,
    /// The OID the entry was built at (blob hash for patch entries). For a
    /// derived entry this stays the entry's own pin — the tracked object —
    /// not the reconstruction that was merged in its place; `derived` records
    /// that.
    pub oid: String,
    pub status: Status,
    #[serde(default)]
    pub conflicted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    /// Blob hash of the coherence fixup applied at the end of this entry's
    /// step, when it carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixup: Option<String>,
    /// What reconstructing a derived entry produced, when this entry is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derived: Option<DerivedResult>,
}

/// How an entry's step ended.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Merged into the stack (a derived entry: its reconstruction was).
    Merged,
    /// Already contained in the base; nothing merged.
    Absorbed,
    /// Merged, but the tree did not change.
    Empty,
    /// A patch entry, applied.
    Applied,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Merged => "merged",
            Status::Absorbed => "absorbed",
            Status::Empty => "empty",
            Status::Applied => "applied",
        }
    }
}

impl EntryResult {
    /// A result with nothing yet to say about conflicts, fixups, or
    /// reconstruction.
    pub fn new(name: &str, oid: String, status: Status) -> EntryResult {
        EntryResult {
            name: name.to_string(),
            oid,
            status,
            conflicted: false,
            resolution: None,
            fixup: None,
            derived: None,
        }
    }
}

/// The two commits a reconstruction produced. Neither is reproducible as an
/// OID — generated commits keep real timestamps — but both are reproducible as
/// trees, which is the invariant this project verifies.
///
/// `conflicted` and `resolution` on the surrounding result describe the entry's
/// merge into the stack. Conflicts met while reconstructing are counted in the
/// build's conflict total and attributed to this entry in the rerere index.
#[derive(Serialize, Deserialize, Clone)]
pub struct DerivedResult {
    /// The merge of the pinned base and every parent pin: where the entry's
    /// own commits are replayed from.
    pub base_tip: String,
    /// The reconstructed head that was merged into the stack.
    pub tip: String,
}

pub fn load(root: &Path) -> Result<Option<Lock>> {
    let path = root.join(FILE);
    if !path.exists() {
        return Ok(None);
    }
    let lock = serde_json::from_str(&fs::read_to_string(&path)?)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(lock))
}

pub fn save(root: &Path, lock: &Lock) -> Result<()> {
    let path = root.join(FILE);
    let mut body = serde_json::to_string_pretty(lock)?;
    body.push('\n');
    fs::write(&path, body).with_context(|| format!("writing {}", path.display()))
}

/// `fixups` maps entry name -> fixup blob hash (see `engine::fixup_blobs`);
/// entries without a fixup are simply absent.
pub fn snapshot(
    entries: &[Entry],
    pins: &Pins,
    fixups: &BTreeMap<String, String>,
) -> Vec<SnapshotEntry> {
    entries
        .iter()
        .map(|entry| SnapshotEntry {
            name: entry.name.clone(),
            kind: entry.kind.kind_str().to_string(),
            source: entry.source(),
            pin: pins.entries.get(&entry.name).cloned().unwrap_or_default(),
            fixup: fixups.get(&entry.name).cloned(),
            reconstruction_publish: entry
                .reconstruction_publish
                .as_ref()
                .map(|target| target.source()),
            parents: entry
                .parents
                .iter()
                .map(|parent| ParentPin {
                    source: parent.source(),
                    pin: pins
                        .parents
                        .get(&entry.name)
                        .and_then(|by_name| by_name.get(&parent.name))
                        .cloned()
                        .unwrap_or_default(),
                })
                .collect(),
            anchor: if entry.parents.is_empty() {
                None
            } else {
                pins.anchors.get(&entry.name).cloned()
            },
        })
        .collect()
}

pub enum Prefix {
    /// No completed build recorded.
    NoBuild,
    /// Manifest and pins exactly match the last build.
    Exact,
    /// Last build is an exact prefix; the new suffix starts at this index.
    Extension(usize),
    /// The prefix no longer matches (reason).
    Diverged(String),
}

/// Why one snapshot entry no longer matches the locked one.
///
/// Every repair cycle has an ordinary shape — an edited fixup, a repinned
/// parent, a re-anchored derivation — and each of them invalidates the suffix
/// through the same comparison. Naming the single field that moved, when
/// exactly one did, is the difference between an operator who knows what to do
/// next and one who re-reads the lock by hand.
fn diverged_reason(index: usize, current: &SnapshotEntry, locked: &SnapshotEntry) -> String {
    let position = index + 1;
    let name = &locked.name;
    // The caller has already established that the two differ, so a probe that
    // takes one field from the locked snapshot and lands on equality proves
    // that field was the whole of the difference.
    let sole = |take: fn(&mut SnapshotEntry, &SnapshotEntry)| -> bool {
        let mut probe = current.clone();
        take(&mut probe, locked);
        probe == *locked
    };
    if sole(|probe, locked| probe.fixup.clone_from(&locked.fixup)) {
        return format!("entry {position} ({name})'s fixup changed");
    }
    if sole(|probe, locked| {
        probe
            .reconstruction_publish
            .clone_from(&locked.reconstruction_publish)
    }) {
        return format!("entry {position} ({name})'s reconstruction publish target changed");
    }
    if sole(|probe, locked| probe.parents.clone_from(&locked.parents)) {
        return format!("entry {position} ({name})'s parent pins moved");
    }
    if sole(|probe, locked| probe.anchor.clone_from(&locked.anchor)) {
        return format!("entry {position} ({name})'s anchor moved");
    }
    format!("entry {position} ({name}) changed, moved, or was repinned since the last build")
}

/// How the current manifest+pins relate to the lock's last build.
pub fn prefix_relation(lock: &Lock, current: &[SnapshotEntry], base_pin: &str) -> Prefix {
    let Some(build) = &lock.build else {
        return Prefix::NoBuild;
    };
    if build.base != base_pin {
        return Prefix::Diverged(format!(
            "base pin moved ({} -> {})",
            git::short(&build.base),
            git::short(base_pin)
        ));
    }
    if build.results.len() != build.manifest_entries.len() {
        return Prefix::Diverged("the lock's results do not match its manifest snapshot".into());
    }
    if current.len() < build.manifest_entries.len() {
        return Prefix::Diverged("entries were removed since the last build".into());
    }
    for (idx, locked) in build.manifest_entries.iter().enumerate() {
        if &current[idx] != locked {
            return Prefix::Diverged(diverged_reason(idx, &current[idx], locked));
        }
    }
    if current.len() == build.manifest_entries.len() {
        Prefix::Exact
    } else {
        Prefix::Extension(build.manifest_entries.len())
    }
}
