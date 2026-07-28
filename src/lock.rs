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
}

#[derive(Serialize, Deserialize, Clone)]
pub struct EntryResult {
    pub name: String,
    /// The OID the entry was built at (blob hash for patch entries).
    pub oid: String,
    /// merged | absorbed | empty | applied
    pub status: String,
    #[serde(default)]
    pub conflicted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
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

pub fn snapshot(entries: &[Entry], pins: &BTreeMap<String, String>) -> Vec<SnapshotEntry> {
    entries
        .iter()
        .map(|entry| SnapshotEntry {
            name: entry.name.clone(),
            kind: entry.kind.kind_str().to_string(),
            source: entry.source(),
            pin: pins.get(&entry.name).cloned().unwrap_or_default(),
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

/// How the current manifest+pins relate to the lock's last build.
pub fn prefix_relation(lock: &Lock, current: &[SnapshotEntry], base_pin: &str) -> Prefix {
    let Some(build) = &lock.build else {
        return Prefix::NoBuild;
    };
    if build.base != base_pin {
        return Prefix::Diverged(format!(
            "base pin moved ({} -> {})",
            &build.base[..12.min(build.base.len())],
            &base_pin[..12.min(base_pin.len())]
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
            return Prefix::Diverged(format!(
                "entry {} ({}) changed, moved, or was repinned since the last build",
                idx + 1,
                locked.name
            ));
        }
    }
    if current.len() == build.manifest_entries.len() {
        Prefix::Exact
    } else {
        Prefix::Extension(build.manifest_entries.len())
    }
}
