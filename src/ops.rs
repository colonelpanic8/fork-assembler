//! Non-build verbs: update (the only pin-mover), status, prune, remove,
//! fixup. Each returns what it found or did; `output` decides how to say it.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Result};
use serde::Serialize;

use crate::engine::derive::{self, Anchor};
use crate::engine::{self, refuse, Ctx};
use crate::git;
use crate::lock::{self, EntryResult, Prefix, Status};
use crate::manifest::edit::{self, Removal};
use crate::manifest::{self, Entry, Exclusion};
use crate::report::Report;
use crate::rerere;
use crate::source;
use crate::state;

/// How a pin moved: nothing recorded before, the same OID, or a new one.
#[derive(Serialize)]
pub struct PinMove {
    pub old: Option<String>,
    pub new: String,
}

#[derive(Serialize)]
pub struct ParentUpdate {
    pub parent: String,
    pub pin: PinMove,
}

#[derive(Serialize)]
pub struct EntryUpdate {
    pub entry: String,
    pub pin: PinMove,
    /// A derived entry's parents, then the anchor re-established from them.
    pub parents: Vec<ParentUpdate>,
    pub anchor: Option<Anchor>,
}

#[derive(Serialize)]
pub struct UpdateReport {
    pub base: Option<PinMove>,
    pub entries: Vec<EntryUpdate>,
}

/// Repin the base and entries to live heads. `build` never moves existing
/// pins; this is the only verb that does.
pub fn update(root: &Path, names: &[String], report: &dyn Report) -> Result<UpdateReport> {
    let ctx = Ctx::open(root, true, report)?;
    let mut lock = lock::load(root)?.unwrap_or_default();
    let all = names.is_empty();
    let wants = |name: &str| all || names.iter().any(|n| n == name);

    for name in names {
        if name != "base" && !ctx.manifest.has_entry(name) {
            bail!("no entry named {name:?} in the manifest");
        }
    }

    let mut base = None;
    if wants("base") {
        let new = engine::fetch_base(&ctx)?;
        base = Some(PinMove {
            old: lock.pins.base.replace(new.clone()),
            new,
        });
    }

    let mut entries = Vec::new();
    for entry in &ctx.manifest.entries {
        if !wants(&entry.name) {
            continue;
        }
        let new = engine::fetch_entry(&ctx, entry)?;
        let old = lock.pins.entries.insert(entry.name.clone(), new.clone());
        let (parents, anchor) = if entry.is_derived() {
            let (parents, anchor) = update_derived(&ctx, &mut lock, entry, &new)?;
            (parents, Some(anchor))
        } else {
            (Vec::new(), None)
        };
        entries.push(EntryUpdate {
            entry: entry.name.clone(),
            pin: PinMove { old, new },
            parents,
            anchor,
        });
    }

    lock::save(root, &lock)?;
    Ok(UpdateReport { base, entries })
}

/// Repin a derived entry's parents and re-establish its anchor, in that order.
///
/// Both belong to `update` for the same reason the entry pin does: they say
/// what the next build reconstructs from, and a `build` that moved them could
/// produce a different tree from the same lock. The anchor comes last because
/// two of its three rules are questions about the pins this just moved, and
/// which rule fired is reported because the anchor decides which commits
/// count as the entry's own — an operator who cannot audit that boundary
/// cannot tell duplicated work from replayed work until it is in the tree.
fn update_derived(
    ctx: &Ctx,
    lock: &mut lock::Lock,
    entry: &Entry,
    pin: &str,
) -> Result<(Vec<ParentUpdate>, Anchor)> {
    let mut pins = lock
        .pins
        .parents
        .get(&entry.name)
        .cloned()
        .unwrap_or_default();
    let mut moves = Vec::new();
    for parent in &entry.parents {
        let new = engine::fetch_parent(ctx, entry, parent)?;
        let old = pins.insert(parent.name.clone(), new.clone());
        moves.push(ParentUpdate {
            parent: parent.name.clone(),
            pin: PinMove { old, new },
        });
    }
    pins.retain(|name, _| entry.parents.iter().any(|p| &p.name == name));

    let previous_base_tip = lock
        .build
        .as_ref()
        .and_then(|b| b.results.iter().find(|r| r.name == entry.name))
        .and_then(|r| r.derived.as_ref())
        .map(|d| d.base_tip.clone());
    let anchor = derive::resolve_anchor(
        &ctx.repo,
        entry,
        pin,
        lock.pins.base.as_deref(),
        &pins,
        lock.pins.anchors.get(&entry.name).map(String::as_str),
        previous_base_tip.as_deref(),
    )?;
    lock.pins.parents.insert(entry.name.clone(), pins);
    lock.pins
        .anchors
        .insert(entry.name.clone(), anchor.oid.clone());
    Ok((moves, anchor))
}

/// True when the pinned topic is contained in the pinned base: a true merge
/// (ancestor) or a squash/rebase equivalent (every topic commit has an
/// upstream patch-id equivalent per `git cherry`).
fn contained(repo: &Path, oid: &str, base: &str) -> bool {
    if git::is_ancestor(repo, oid, base) {
        return true;
    }
    match git::out(repo, &["cherry", base, oid]) {
        Ok(out) => {
            let lines: Vec<&str> = out.lines().collect();
            !lines.is_empty() && lines.iter().all(|l| l.starts_with('-'))
        }
        Err(_) => false,
    }
}

/// Something `status` has to say about one pinned topic.
#[derive(Serialize)]
#[serde(tag = "flag", rename_all = "snake_case")]
pub enum Flag {
    /// A tracked rerere pair is attributed to this entry.
    RerereResolution,
    Fixup {
        path: String,
    },
    FixupMissing {
        path: String,
    },
    /// The last build did not simply merge or apply it.
    LastBuild {
        status: Status,
    },
    /// The base already contains it.
    ContainedInBase,
    /// It cannot merge with the base on its own; `build` refuses it.
    ConflictsWithBase,
    /// The live ref has moved past the pin.
    LiveHead {
        oid: String,
    },
}

#[derive(Serialize)]
pub struct ParentStatus {
    pub name: String,
    pub source: String,
    pub pin: Option<String>,
    pub flags: Vec<Flag>,
}

#[derive(Serialize)]
pub struct DerivedStatus {
    pub parents: Vec<ParentStatus>,
    /// None until a build has detected it.
    pub anchor: Option<String>,
}

#[derive(Serialize)]
pub struct EntryStatus {
    pub name: String,
    pub source: String,
    pub pin: Option<String>,
    pub flags: Vec<Flag>,
    pub derived: Option<DerivedStatus>,
}

#[derive(Serialize)]
pub struct BaseStatus {
    /// `remote:ref`.
    pub source: String,
    pub pin: Option<String>,
    /// The fetched head, when `--live` asked for it.
    pub live_head: Option<String>,
}

#[derive(Serialize)]
pub struct LastBuild {
    pub commit: String,
    pub tree: String,
    pub conflicts: u32,
    /// How the current manifest and pins relate to it.
    #[serde(flatten)]
    pub relation: Prefix,
}

#[derive(Serialize)]
pub struct StatusReport {
    pub base: BaseStatus,
    pub entries: Vec<EntryStatus>,
    pub excludes: Vec<Exclusion>,
    pub last_build: Option<LastBuild>,
}

/// How a pinned topic stands against the pinned base.
fn base_flag(repo: &Path, pin: &str, base: &str) -> Option<Flag> {
    if !git::has_commit(repo, pin) {
        None
    } else if contained(repo, pin, base) {
        Some(Flag::ContainedInBase)
    } else if refuse::conflicts_with_base(repo, base, pin) {
        // Reported here because `build` refuses this outright, and the
        // repair is in another repository. Better to learn it from a
        // status read than from a build that stops and rolls back.
        Some(Flag::ConflictsWithBase)
    } else {
        None
    }
}

pub fn status(root: &Path, live: bool, report: &dyn Report) -> Result<StatusReport> {
    let m = manifest::load(root)?;
    let lock = lock::load(root)?.unwrap_or_default();
    // Live checks fetch, so they need a configured source; offline ones
    // only need whatever repository already exists.
    let repo = if live {
        Some(source::source_repo(root, &m, true, report)?)
    } else {
        source::source_repo_if_present(root, &m)
    };
    let ctx = repo
        .as_ref()
        .filter(|_| live)
        .map(|repo| Ctx::new(root, m.clone(), repo.clone(), report));

    let base = BaseStatus {
        source: format!("{}:{}", m.base.remote, m.base.ref_),
        pin: lock.pins.base.clone(),
        live_head: ctx.as_ref().map(engine::fetch_base).transpose()?,
    };

    let results: BTreeMap<&str, &EntryResult> = lock
        .build
        .as_ref()
        .map(|b| b.results.iter().map(|r| (r.name.as_str(), r)).collect())
        .unwrap_or_default();
    let recorded = rerere::index_entry_names(root)?;
    let base_pin = lock.pins.base.as_deref();

    let mut entries = Vec::new();
    for entry in &m.entries {
        let pin = lock.pins.entries.get(&entry.name);
        let mut flags = Vec::new();
        if recorded.contains(&entry.name) {
            flags.push(Flag::RerereResolution);
        }
        if let Some(path) = &entry.fixup {
            flags.push(if root.join(path).exists() {
                Flag::Fixup { path: path.clone() }
            } else {
                Flag::FixupMissing { path: path.clone() }
            });
        }
        if let Some(result) = results.get(entry.name.as_str()) {
            if !matches!(result.status, Status::Merged | Status::Applied) {
                flags.push(Flag::LastBuild {
                    status: result.status,
                });
            }
        }
        if let (Some(pin), Some(base), Some(repo), false) =
            (pin, base_pin, repo.as_ref(), entry.kind.is_patch())
        {
            flags.extend(base_flag(repo, pin, base));
        }
        if let (Some(ctx), false) = (ctx.as_ref(), entry.kind.is_patch()) {
            let head = engine::fetch_entry(ctx, entry)?;
            if Some(&head) != pin {
                flags.push(Flag::LiveHead { oid: head });
            }
        }

        let derived = if entry.is_derived() {
            let mut parents = Vec::new();
            for parent in &entry.parents {
                let pin = lock
                    .pins
                    .parents
                    .get(&entry.name)
                    .and_then(|pins| pins.get(&parent.name));
                let mut flags = Vec::new();
                if let (Some(pin), Some(base), Some(repo)) = (pin, base_pin, repo.as_ref()) {
                    flags.extend(base_flag(repo, pin, base));
                }
                if let Some(ctx) = ctx.as_ref() {
                    let head = engine::fetch_parent(ctx, entry, parent)?;
                    if Some(&head) != pin {
                        flags.push(Flag::LiveHead { oid: head });
                    }
                }
                parents.push(ParentStatus {
                    name: parent.name.clone(),
                    source: parent.source(),
                    pin: pin.cloned(),
                    flags,
                });
            }
            Some(DerivedStatus {
                parents,
                anchor: lock.pins.anchors.get(&entry.name).cloned(),
            })
        } else {
            None
        };
        entries.push(EntryStatus {
            name: entry.name.clone(),
            source: entry.source(),
            pin: pin.cloned(),
            flags,
            derived,
        });
    }

    let last_build = match &lock.build {
        Some(build) => {
            let fixups = engine::fixup_blobs(root, &m.entries, false)?;
            let snapshot = lock::snapshot(&m.entries, &lock.pins, &fixups);
            Some(LastBuild {
                commit: build.commit.clone(),
                tree: build.tree.clone(),
                conflicts: build.conflicts,
                relation: lock::prefix_relation(&lock, &snapshot, base_pin.unwrap_or_default()),
            })
        }
        None => None,
    };
    Ok(StatusReport {
        base,
        entries,
        excludes: m.excludes,
        last_build,
    })
}

#[derive(Serialize)]
pub struct Contained {
    pub entry: String,
    pub pin: String,
}

#[derive(Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Pruned {
    /// No pinned entry is contained in the base.
    Nothing,
    DryRun,
    Removed {
        removal: Removal,
    },
}

#[derive(Serialize)]
pub struct PruneReport {
    pub contained: Vec<Contained>,
    pub outcome: Pruned,
}

pub fn prune(root: &Path, dry_run: bool) -> Result<PruneReport> {
    let m = manifest::load(root)?;
    let lock = lock::load(root)?.unwrap_or_default();
    let Some(base) = lock.pins.base.clone() else {
        bail!("no base pin; run `fork-assembler build` or `update` first");
    };
    let Some(repo) = source::source_repo_if_present(root, &m) else {
        bail!("no source repository available; run `fork-assembler build` first");
    };

    let mut contained_entries = Vec::new();
    for entry in &m.entries {
        if entry.kind.is_patch() {
            continue;
        }
        let Some(pin) = lock.pins.entries.get(&entry.name) else {
            continue;
        };
        if git::has_commit(&repo, pin) && contained(&repo, pin, &base) {
            contained_entries.push(Contained {
                entry: entry.name.clone(),
                pin: pin.clone(),
            });
        }
    }
    let outcome = if contained_entries.is_empty() {
        Pruned::Nothing
    } else if dry_run {
        Pruned::DryRun
    } else {
        let dead: Vec<String> = contained_entries.iter().map(|c| c.entry.clone()).collect();
        Pruned::Removed {
            removal: edit::remove_entries(root, &dead)?,
        }
    };
    Ok(PruneReport {
        contained: contained_entries,
        outcome,
    })
}

pub fn remove(root: &Path, name: String) -> Result<Removal> {
    edit::remove_entries(root, &[name])
}

/// Where a captured fixup's diff came from.
#[derive(Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum CaptureSource {
    /// The build worktree's uncommitted changes: the state at a fixup
    /// stall, which is precisely the corrected fixup.
    Uncommitted,
    /// The entry's already-committed fixup commit: the state after
    /// `continue` committed a manual resolution.
    Commit { oid: String, subject: String },
}

#[derive(Serialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum FixupReport {
    Detached {
        entry: String,
        /// The entry's position: the suffix a rebuild must redo.
        index: usize,
    },
    Set {
        entry: String,
        index: usize,
        path: String,
        captured: Option<CaptureSource>,
    },
}

/// Attach, re-capture, or detach an entry's coherence fixup.
pub fn fixup(
    root: &Path,
    name: &str,
    path: Option<&str>,
    capture: bool,
    remove: bool,
) -> Result<FixupReport> {
    if remove {
        let index = edit::set_fixup(root, name, None)?;
        return Ok(FixupReport::Detached {
            entry: name.to_string(),
            index,
        });
    }
    let Some(rel) = path else {
        bail!("pass the fixup's patch path, or --remove to detach it");
    };

    let mut captured = None;
    if capture {
        let worktree = root.join(engine::WORKTREE);
        if !worktree.exists() {
            bail!(
                "no build worktree at {} to capture from; run `fork-assembler build` first",
                worktree.display()
            );
        }
        let dest = root.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let Some((diff, source)) = capture_diff(&worktree, name)? else {
            bail!(
                "nothing to capture: the build worktree has no uncommitted changes and no \
                 `fork-assembler: fixup {name}` commit"
            );
        };
        std::fs::write(&dest, diff)?;
        captured = Some(source);
        // A capture during a stalled build supersedes that build: the fixup
        // blob just changed, so the stalled worktree can only be rebuilt.
        let _ = state::clear(&worktree);
    } else if !root.join(rel).exists() {
        bail!(
            "patch file {rel} does not exist (pass --capture to write it from the build worktree)"
        );
    }

    let index = edit::set_fixup(root, name, Some(rel))?;
    Ok(FixupReport::Set {
        entry: name.to_string(),
        index,
        path: rel.to_string(),
        captured,
    })
}

/// The diff a capture should record, and where it came from: the build
/// worktree's uncommitted changes when there are any, else the entry's
/// already-committed fixup commit. None when there is neither.
fn capture_diff(worktree: &Path, name: &str) -> Result<Option<(Vec<u8>, CaptureSource)>> {
    // Intent-to-add so newly created files appear in the diff.
    let _ = git::raw(worktree, &["add", "-A", "-N"]);
    let pending = git::bytes(worktree, &["diff", "--binary", "HEAD"])?;
    if !pending.is_empty() {
        return Ok(Some((pending, CaptureSource::Uncommitted)));
    }
    let subject = format!("fork-assembler: fixup {name}");
    let log = git::out(worktree, &["log", "--format=%H%x1f%s"])?;
    let commit = log
        .lines()
        .find_map(|line| line.split_once('\x1f').filter(|(_, s)| *s == subject))
        .map(|(hash, _)| hash.to_string());
    let Some(commit) = commit else {
        return Ok(None);
    };
    let diff = git::bytes(worktree, &["show", "--binary", "--format=", &commit])?;
    Ok(Some((
        diff,
        CaptureSource::Commit {
            oid: commit,
            subject,
        },
    )))
}
