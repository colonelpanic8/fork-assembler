//! The build engine: assemble the stack from the lock's pins.
//!
//! `build` never moves an existing pin (`update` is the only verb that does);
//! entries with no pin yet get pinned from live refs on their first build.
//! The assembled branch is compiled output. `tree` (pre-provenance) is the
//! reproducibility invariant.
//!
//! This module owns `Run`, the entry loop, and its two entry points `build`
//! and `cont`. Pin resolution, derived-entry reconstruction, the base-conflict
//! refusal, and the provenance record each live in a submodule. Nothing here
//! writes to stdout: the run emits `report::Event`s and the `Ctx`'s reporter
//! decides what they look like.

pub mod derive;
pub mod pins;
pub mod provenance;
pub mod refuse;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;

use crate::git;
use crate::lock::{self, EntryResult, Lock, Prefix, Status};
use crate::manifest::{self, Entry, Kind, Manifest};
use crate::report::{Event, Outcome, Report, Resolved};
use crate::rerere;
use crate::source;
use crate::state::{self, State};

pub use pins::{fetch_base, fetch_entry, fetch_parent, fixup_blobs, patch_blob};

pub const WORKTREE: &str = ".worktrees/build";

/// Where derived entries are reconstructed. A second worktree, because
/// reconstruction is a merge sequence of its own that must not disturb the
/// half-assembled stack it will eventually be merged into.
pub const DERIVE_WORKTREE: &str = ".worktrees/derive";

/// Exit code signalling "stopped for human resolution".
pub const STOPPED: i32 = 2;

pub struct Ctx<'r> {
    pub root: PathBuf,
    pub manifest: Manifest,
    pub repo: PathBuf,
    pub worktree: PathBuf,
    pub report: &'r dyn Report,
}

impl<'r> Ctx<'r> {
    pub fn new(root: &Path, manifest: Manifest, repo: PathBuf, report: &'r dyn Report) -> Ctx<'r> {
        Ctx {
            root: root.to_path_buf(),
            worktree: root.join(WORKTREE),
            manifest,
            repo,
            report,
        }
    }

    pub fn open(root: &Path, allow_clone: bool, report: &'r dyn Report) -> Result<Ctx<'r>> {
        let manifest = manifest::load(root)?;
        let repo = source::source_repo(root, &manifest, allow_clone, report)?;
        Ok(Ctx::new(root, manifest, repo, report))
    }

    pub fn derive_worktree(&self) -> PathBuf {
        self.root.join(DERIVE_WORKTREE)
    }

    pub fn emit(&self, event: Event) {
        self.report.event(&event)
    }

    /// The entry at `index`, as one step of the run.
    pub fn step(&self, index: usize) -> Step<'_> {
        Step {
            index,
            total: self.manifest.entries.len(),
            entry: &self.manifest.entries[index],
        }
    }
}

/// The entry a run is working on.
pub struct Step<'a> {
    pub index: usize,
    pub total: usize,
    pub entry: &'a Entry,
}

/// One in-progress build: the context it runs in and the state it persists
/// after every step. `build` starts one; `cont` picks one back up.
pub struct Run<'a> {
    pub ctx: &'a Ctx<'a>,
    pub st: State,
}

fn remove_worktree(ctx: &Ctx, path: &Path) {
    if path.exists() {
        let _ = git::raw(
            &ctx.repo,
            &["worktree", "remove", "--force", &path.to_string_lossy()],
        );
    }
    // A deleted-but-registered worktree (e.g. the directory was rm -rf'd)
    // blocks re-adding at the same path.
    let _ = git::raw(&ctx.repo, &["worktree", "prune"]);
}

fn prepare_worktree(ctx: &Ctx, path: &Path, at: &str) -> Result<()> {
    remove_worktree(ctx, path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    git::out(
        &ctx.repo,
        &["worktree", "add", "--detach", &path.to_string_lossy(), at],
    )?;
    Ok(())
}

fn conflicted_files(worktree: &Path) -> Result<Vec<String>> {
    let out = git::out(worktree, &["diff", "--name-only", "--diff-filter=U"])?;
    Ok(out.lines().map(str::to_string).collect())
}

fn staged_files(worktree: &Path) -> Result<String> {
    git::out(worktree, &["diff", "--cached", "--name-only"])
}

/// How applying a patch file went.
enum Apply {
    Done(Outcome),
    /// Failed, conflict left for the human; git's own account of why.
    Failed(String),
}

/// Apply a tracked patch file and commit it as `message`. Shared by patch
/// entries and coherence fixups.
fn apply_patch_file(ctx: &Ctx, rel: &str, message: &str) -> Result<Apply> {
    let patch = ctx.root.join(rel);
    let patch_str = patch.to_string_lossy().to_string();

    // "Reverse-applies" alone does not mean "already applied": a patch that
    // DELETES a duplicated block reverse-applies against the surviving copy,
    // because the copies are byte-identical and context matching cannot tell
    // them apart. Only trust the reverse check when the patch also cannot be
    // applied forwards.
    let reverse = git::ok(
        &ctx.worktree,
        &["apply", "--reverse", "--check", &patch_str],
    );
    let forward = git::ok(&ctx.worktree, &["apply", "--check", &patch_str]);
    if reverse && !forward {
        return Ok(Apply::Done(Outcome::AlreadyApplied));
    }

    let applied = git::raw(&ctx.worktree, &["apply", "--3way", &patch_str])?;
    if !applied.status.success() {
        let stderr = String::from_utf8_lossy(&applied.stderr)
            .trim_end()
            .to_string();
        return Ok(Apply::Failed(stderr));
    }
    git::out(&ctx.worktree, &["add", "-A"])?;
    if staged_files(&ctx.worktree)?.is_empty() {
        return Ok(Apply::Done(Outcome::AlreadyApplied));
    }
    git::out(&ctx.worktree, &["commit", "-q", "-m", message])?;
    Ok(Apply::Done(Outcome::Applied))
}

impl<'a> Run<'a> {
    pub fn persist(&self) -> Result<()> {
        state::write(&self.ctx.worktree, &self.st)
    }

    fn emit(&self, event: Event) {
        self.ctx.emit(event)
    }

    /// Persist that entry `index` is where the build stopped for the human.
    fn stall(&mut self, index: usize) -> Result<Option<i32>> {
        self.st.next_index = index;
        self.persist()?;
        Ok(Some(STOPPED))
    }

    /// Complete an entry's step: apply its coherence fixup, if any, then
    /// record the result and advance. Applying the fixup HERE — inside the
    /// entry's step, not as a later standalone entry — is what makes every
    /// entry boundary a coherent tree.
    ///
    /// Returns false when the fixup needs a human. The merge is already
    /// committed at that point, so the merge's result is held in
    /// `state.pending`: its presence is how `continue` knows to resume at the
    /// fixup rather than re-running the merge.
    fn finish_entry(&mut self, step: &Step, mut result: EntryResult) -> Result<bool> {
        let ctx = self.ctx;
        let entry = step.entry;
        if let Some(path) = &entry.fixup {
            let blob = patch_blob(&ctx.root, path)?;
            let message = format!("fork-assembler: fixup {}", entry.name);
            match apply_patch_file(ctx, path, &message)? {
                Apply::Done(outcome) => {
                    self.emit(Event::Fixup {
                        step: step.info(),
                        path: path.clone(),
                        outcome,
                    });
                    result.fixup = Some(blob);
                }
                Apply::Failed(stderr) => {
                    self.st.pending = Some(result);
                    self.stall(step.index)?;
                    self.emit(Event::FixupFailed {
                        step: step.info(),
                        path: path.clone(),
                        stderr,
                        worktree: ctx.worktree.clone(),
                    });
                    return Ok(false);
                }
            }
        }
        self.st.results.push(result);
        self.st.pending = None;
        self.st.next_index = step.index + 1;
        self.persist()?;
        Ok(true)
    }

    /// One patch entry's step.
    fn run_patch_entry(&mut self, step: &Step, rel: &str, oid: String) -> Result<Option<i32>> {
        let ctx = self.ctx;
        let message = format!("fork-assembler: {}", step.entry.name);
        let outcome = match apply_patch_file(ctx, rel, &message)? {
            Apply::Done(outcome) => outcome,
            Apply::Failed(stderr) => {
                self.stall(step.index)?;
                self.emit(Event::PatchFailed {
                    step: step.info(),
                    stderr,
                    worktree: ctx.worktree.clone(),
                });
                return Ok(Some(STOPPED));
            }
        };
        self.emit(Event::Applied {
            step: step.info(),
            outcome,
        });
        // Patch entries cannot carry a fixup (the manifest rejects it), so
        // this only records and advances.
        let result = EntryResult::new(&step.entry.name, oid, Status::Applied);
        self.finish_entry(step, result)?;
        Ok(None)
    }

    /// Merge `oid` into the stack as `entry`'s step.
    fn merge(&self, entry: &Entry, oid: &str) -> Result<bool> {
        let message = format!("fork-assembler: merge {}", entry.name);
        let out = git::raw(
            &self.ctx.worktree,
            &rerere::with_cfg(&["merge", "--no-ff", "--no-edit", "-m", &message, oid]),
        )?;
        Ok(out.status.success())
    }

    /// One live entry's step: merge it — or, for a derived entry, its
    /// reconstruction — into the stack, then finish the step with its fixup.
    fn run_live_entry(&mut self, step: &Step<'a>, oid: String) -> Result<Option<i32>> {
        let ctx = self.ctx;
        let entry = step.entry;
        if git::is_ancestor(&ctx.repo, &oid, &self.st.base) {
            self.emit(Event::Absorbed { step: step.info() });
            // A derived entry stops here too, before any reconstruction: the
            // base already contains the whole thing, parents and own commits
            // alike, so there is nothing left to rebuild it out of.
            // The fixup still runs: this entry's content reaching the tree via
            // the base rather than via a merge does not mean the incoherence
            // it repaired went away. If it did, the fixup reports "already
            // applied"; if it did not, it applies as usual.
            let result = EntryResult::new(&entry.name, oid, Status::Absorbed);
            return Ok((!self.finish_entry(step, result)?).then_some(STOPPED));
        }

        // A derived entry's pin is stale by construction: it was built against
        // whatever its parents were then. What gets merged is the
        // reconstruction; what gets recorded is still the pin.
        let derived = if entry.is_derived() {
            match derive::reconstruct(self, step, &oid)? {
                Some(derived) => Some(derived),
                None => return Ok(Some(STOPPED)),
            }
        } else {
            None
        };
        let merging = derived.as_ref().map_or(oid.as_str(), |d| d.tip.as_str());

        let before = git::out(&ctx.worktree, &["rev-parse", "HEAD^{tree}"])?;
        let clean = self.merge(entry, merging)?;
        let mut result = EntryResult::new(&entry.name, oid.clone(), Status::Merged);

        if clean {
            let after = git::out(&ctx.worktree, &["rev-parse", "HEAD^{tree}"])?;
            if before == after {
                self.emit(Event::Empty { step: step.info() });
                result.status = Status::Empty;
            } else {
                self.emit(Event::Merged {
                    step: step.info(),
                    oid: merging.to_string(),
                    reconstruction: derived.is_some(),
                });
            }
        } else {
            // A derived entry merges its reconstruction, which already
            // contains the base; its topics were checked against the base one
            // at a time as they were merged in.
            if !entry.is_derived() {
                refuse::refuse_if_base_conflict(
                    ctx,
                    entry,
                    refuse::Topic::entry(entry, &oid),
                    &self.st.base,
                )?;
            }
            self.st.conflicts += 1;
            let unresolved = conflicted_files(&ctx.worktree)?;
            if !unresolved.is_empty() {
                self.stall(step.index)?;
                self.emit(Event::Conflict {
                    step: step.info(),
                    files: unresolved,
                    worktree: ctx.worktree.clone(),
                });
                return Ok(Some(STOPPED));
            }
            // rerere recognized every conflict hunk and staged the recorded
            // resolutions (autoUpdate); commit and continue.
            let hashes: Vec<String> = rerere::merge_rr(&ctx.worktree)?
                .into_iter()
                .map(|(hash, _)| hash)
                .collect();
            git::out(&ctx.worktree, &rerere::with_cfg(&["commit", "--no-edit"]))?;
            self.emit(Event::AutoResolved { step: step.info() });
            result.conflicted = true;
            result.resolution = Some(rerere::label(&hashes));
        }
        result.derived = derived;
        let done = self.finish_entry(step, result)?;
        derive::clean(ctx, entry, done);
        Ok((!done).then_some(STOPPED))
    }

    /// The core loop: process entries[start..], persisting state after each.
    /// Returns Some(exit_code) when stopped for the human, None when complete.
    fn run_entries(&mut self, start: usize) -> Result<Option<i32>> {
        let ctx = self.ctx;
        for index in start..ctx.manifest.entries.len() {
            let step = ctx.step(index);
            let entry = step.entry;
            let oid = self
                .st
                .pins
                .get(&entry.name)
                .cloned()
                .with_context(|| format!("{}: pin vanished mid-build", entry.name))?;
            let stopped = match &entry.kind {
                Kind::Patch { path } => self.run_patch_entry(&step, path, oid)?,
                _ => self.run_live_entry(&step, oid)?,
            };
            if stopped.is_some() {
                return Ok(stopped);
            }
        }
        Ok(None)
    }

    /// Run the entry loop from `start` and, once it completes, publish
    /// reconstructions and finalize. The process exit code either way.
    fn drive(&mut self, start: usize, previous: Option<&Lock>, locked: bool) -> Result<i32> {
        match self.run_entries(start)? {
            Some(code) => Ok(code),
            None => {
                derive::publish(self, locked)?;
                self.finalize(previous, !locked)?;
                Ok(0)
            }
        }
    }

    /// Finish a completed run: provenance commit, lock write, reporting.
    fn finalize(&self, previous: Option<&Lock>, write_lock: bool) -> Result<()> {
        let (ctx, st) = (self.ctx, &self.st);
        let pre_provenance = git::out(&ctx.worktree, &["rev-parse", "HEAD"])?;
        // The content tree BEFORE the provenance commit: `tree` has to keep
        // meaning "what the topics and base produced", or every rebuild would
        // look changed and the "tree unchanged" signal would die.
        let tree = git::out(&ctx.worktree, &["rev-parse", "HEAD^{tree}"])?;

        let head = if let Some(file) = &ctx.manifest.provenance_file {
            let provenance = provenance::json(self)?;
            let body = serde_json::to_string_pretty(&provenance)? + "\n";
            std::fs::write(ctx.worktree.join(file), body)?;
            git::out(&ctx.worktree, &["add", file])?;
            if !git::out(&ctx.worktree, &["status", "--porcelain", "--", file])?.is_empty() {
                git::out(
                    &ctx.worktree,
                    &[
                        "commit",
                        "-q",
                        "-m",
                        "fork-assembler: record build provenance",
                    ],
                )?;
            }
            git::out(&ctx.worktree, &["rev-parse", "HEAD"])?
        } else {
            pre_provenance.clone()
        };
        let built_tree = git::out(&ctx.worktree, &["rev-parse", "HEAD^{tree}"])?;

        let previous_build = previous.and_then(|l| l.build.as_ref());
        let previous_tree = previous_build.map(|b| b.tree.clone());
        self.emit(Event::Finished {
            tree: tree.clone(),
            commit: head.clone(),
            conflicts: st.conflicts,
            previous_tree: previous_tree.clone(),
        });

        if write_lock {
            let mut lock = previous.cloned().unwrap_or_default();
            lock.pins.base = Some(st.base.clone());
            lock.pins.entries = st.pins.clone();
            // A first build establishes a derived entry's parent pins and
            // anchor; every later one carries forward what it consumed.
            lock.pins.parents.clone_from(&st.parent_pins);
            lock.pins.anchors.clone_from(&st.anchors);
            let manifest_entries = lock::snapshot(
                &ctx.manifest.entries,
                &lock.pins,
                &fixup_blobs(&ctx.root, &ctx.manifest.entries, true)?,
            );
            lock.build = Some(lock::BuildRecord {
                generated: Utc::now().to_rfc3339(),
                base: st.base.clone(),
                commit: head,
                pre_provenance_commit: pre_provenance,
                tree: tree.clone(),
                built_tree,
                tree_changed: previous_tree.as_deref() != Some(tree.as_str()),
                previous_tree,
                conflicts: st.conflicts,
                manifest_entries,
                results: st.results.clone(),
            });
            lock::save(&ctx.root, &lock)?;
            self.emit(Event::LockWritten);
        } else {
            match previous_tree {
                Some(expected) if expected == tree => self.emit(Event::Verified),
                Some(expected) => {
                    bail!("reproduction FAILED: built tree {tree} but the lock records {expected}")
                }
                None => self.emit(Event::NothingToVerify),
            }
        }
        state::clear(&ctx.worktree)?;
        Ok(())
    }

    /// Resume a build stalled in a coherence fixup: the merge is already
    /// committed, so only the fixup's staged content needs a commit.
    /// Re-running the merge here would duplicate it.
    fn resume_fixup(&mut self, mut pending: EntryResult) -> Result<()> {
        let ctx = self.ctx;
        let step = ctx.step(self.st.next_index);
        let entry = step.entry;
        let path = entry.fixup.clone().with_context(|| {
            format!(
                "{}: the build stalled in a coherence fixup but the manifest no longer \
                 declares one; abandon the worktree and rebuild",
                entry.name
            )
        })?;
        if staged_files(&ctx.worktree)?.is_empty() {
            bail!(
                "{}: nothing staged for the fixup {path}; resolve it and `git add`, \
                 or detach it with `fork-assembler fixup {} --remove`",
                entry.name,
                entry.name
            );
        }
        let message = format!("fork-assembler: fixup {}", entry.name);
        git::out(&ctx.worktree, &["commit", "-q", "-m", &message])?;
        pending.fixup = Some(patch_blob(&ctx.root, &path)?);
        self.st.results.push(pending);
        self.st.next_index = step.index + 1;
        self.persist()?;
        // The lock now pins a fixup blob whose patch does NOT reproduce what
        // was just committed, so a rebuild stalls here again; the event says
        // so.
        self.emit(Event::FixupCommitted {
            step: step.info(),
            path,
        });
        Ok(())
    }

    /// Resume a build stalled on a stack merge the human has resolved: commit
    /// it, harvest the rerere pairs, and finish the entry's step. Returns
    /// false when the step's fixup then stalled in turn.
    fn resume_merge(&mut self) -> Result<bool> {
        let ctx = self.ctx;
        let step = ctx.step(self.st.next_index);
        let entry = step.entry;
        // Read MERGE_RR before committing: the rerere-enabled commit records
        // the postimages and clears it.
        let merge_rr = rerere::merge_rr(&ctx.worktree)?;
        git::out(&ctx.worktree, &rerere::with_cfg(&["commit", "--no-edit"]))?;
        let harvested = rerere::harvest(&ctx.root, &ctx.worktree)?;
        rerere::index_add(&ctx.root, &entry.name, &harvested, &merge_rr)?;
        // The tracked object, which is not always the merged one: a derived
        // entry merges its reconstruction, and the lock records its pin.
        let oid = match self.st.pins.get(&entry.name) {
            Some(pin) => pin.clone(),
            None => git::out(&ctx.worktree, &["rev-parse", "HEAD^2"])?,
        };
        self.emit(Event::Harvested {
            step: step.info(),
            resolved: Resolved::StackMerge,
            pairs: harvested.clone(),
        });
        let mut result = EntryResult::new(&entry.name, oid, Status::Merged);
        result.conflicted = true;
        result.resolution = Some(rerere::label(&harvested));
        result.derived = self.st.derived.get(&entry.name).cloned();
        // finish_entry persists the advance IMMEDIATELY: if the very next
        // entry errors, a stale index would re-merge this one and falsely
        // report it EMPTY. It also runs this entry's fixup, which can stall
        // in turn — the resolution and its fixup are one step.
        let done = self.finish_entry(&step, result)?;
        derive::clean(ctx, entry, done);
        Ok(done)
    }

    /// Resume a build stalled on a patch entry: commit what the human staged.
    fn resume_patch(&mut self) -> Result<()> {
        let ctx = self.ctx;
        let step = ctx.step(self.st.next_index);
        let entry = step.entry;
        if !entry.kind.is_patch() {
            return Ok(());
        }
        let message = format!("fork-assembler: {}", entry.name);
        git::out(&ctx.worktree, &["commit", "-q", "-m", &message])?;
        let oid = self.st.pins.get(&entry.name).cloned().unwrap_or_default();
        let mut result = EntryResult::new(&entry.name, oid, Status::Applied);
        result.conflicted = true;
        self.st.results.push(result);
        self.st.next_index = step.index + 1;
        self.persist()
    }
}

/// The pinned base, fetched once if the object is missing, or pinned fresh
/// when nothing has pinned it yet.
fn ensure_base(ctx: &Ctx, pinned: Option<String>, locked: bool) -> Result<String> {
    let Some(base) = pinned else {
        if locked {
            bail!("no base pin recorded and --locked forbids pinning");
        }
        let oid = fetch_base(ctx)?;
        ctx.emit(Event::PinnedBase { oid: oid.clone() });
        return Ok(oid);
    };
    if !git::has_commit(&ctx.repo, &base) {
        if locked {
            bail!("pinned base {base} is not present locally and --locked forbids fetching");
        }
        fetch_base(ctx)?;
        if !git::has_commit(&ctx.repo, &base) {
            bail!("pinned base {base} is not reachable from the live base ref");
        }
    }
    Ok(base)
}

pub fn build(root: &Path, locked: bool, report: &dyn Report) -> Result<i32> {
    let ctx = Ctx::open(root, !locked, report)?;
    if state::read(&ctx.worktree).is_ok_and(|st| st.is_some()) {
        bail!(
            "a build is already in progress in {}; finish it with `fork-assembler continue` \
             (or remove the worktree to abandon it)",
            ctx.worktree.display()
        );
    }

    let previous = lock::load(&ctx.root)?;
    let mut pins = previous
        .as_ref()
        .map(|l| l.pins.clone())
        .unwrap_or_default();
    let base = ensure_base(&ctx, pins.base.take(), locked)?;

    for entry in &ctx.manifest.entries {
        pins::ensure_pin(&ctx, entry, &mut pins.entries, locked)?;
        if entry.is_derived() {
            pins::ensure_parent_pins(&ctx, entry, &mut pins.parents, locked)?;
        }
    }
    // Pins for entries the manifest no longer carries are not facts about
    // anything; carrying them forward would leave the lock asserting
    // relationships that nothing declares — and a stale entry pin is actively
    // misleading when the same ref later reappears as a parent, pinned
    // elsewhere at a different OID. A removed entry that returns simply pins
    // fresh, which is what its first build would do anyway.
    pins.entries.retain(|name, _| ctx.manifest.has_entry(name));
    pins.parents.retain(|name, _| ctx.manifest.has_entry(name));
    pins.anchors.retain(|name, _| ctx.manifest.has_entry(name));

    // Decide full rebuild vs incremental extension vs up-to-date. Fixup blobs
    // ride in the snapshot, so editing one invalidates from its entry exactly
    // as repinning that entry would.
    let fixups = fixup_blobs(&ctx.root, &ctx.manifest.entries, true)?;
    let snapshot = lock::snapshot(&ctx.manifest.entries, &pins, &fixups);
    let relation = previous
        .as_ref()
        .map(|l| lock::prefix_relation(l, &snapshot, &base))
        .unwrap_or(Prefix::NoBuild);
    let last_build = previous.as_ref().and_then(|l| l.build.as_ref());

    // Where the run starts: the last build's head when this one extends it,
    // else the base. A locked build always reproduces from scratch.
    let mut start = 0usize;
    let mut results: Vec<EntryResult> = Vec::new();
    let mut extended_from = None;
    let mut start_commit = base.clone();
    match (relation, last_build) {
        (Prefix::Exact, Some(build))
            if !locked && git::has_commit(&ctx.repo, &build.pre_provenance_commit) =>
        {
            ctx.emit(Event::UpToDate {
                tree: build.tree.clone(),
            });
            return Ok(0);
        }
        (Prefix::Extension { at }, Some(build))
            if !locked && git::has_commit(&ctx.repo, &build.pre_provenance_commit) =>
        {
            ctx.emit(Event::Extending {
                new_entries: snapshot.len() - at,
            });
            start = at;
            results = build.results.clone();
            extended_from = Some(build.pre_provenance_commit.clone());
            start_commit = build.pre_provenance_commit.clone();
        }
        _ => {}
    }
    if start == 0 {
        ctx.emit(Event::FromBase { base: base.clone() });
    }

    prepare_worktree(&ctx, &ctx.worktree, &start_commit)?;
    // Any reconstruction worktree still here belongs to a build that is over:
    // this one refused to start while state existed.
    remove_worktree(&ctx, &ctx.derive_worktree());
    // One seeding covers both worktrees: rr-cache lives in the source repo's
    // common git dir, which every worktree of it shares.
    let seeded = rerere::seed(&ctx.root, &ctx.worktree)?;
    if seeded > 0 {
        ctx.emit(Event::Seeded { pairs: seeded });
    }
    let mut run = Run {
        ctx: &ctx,
        st: State {
            next_index: start,
            base,
            pins: pins.entries,
            parent_pins: pins.parents,
            anchors: pins.anchors,
            results,
            conflicts: 0,
            extended_from,
            pending: None,
            derive: None,
            derived: BTreeMap::new(),
        },
    };
    run.persist()?;
    run.drive(start, previous.as_ref(), locked)
}

pub fn cont(root: &Path, report: &dyn Report) -> Result<i32> {
    let ctx = Ctx::open(root, true, report)?;
    let Some(st) = state::read(&ctx.worktree)? else {
        bail!("no in-progress build found; run `fork-assembler build`");
    };
    let mut run = Run { ctx: &ctx, st };

    // A reconstruction in flight owns the conflict: the build worktree is
    // merely paused behind it, and resolving there would repair nothing.
    let in_derive = run.st.derive.is_some();
    let stalled_worktree = if in_derive {
        ctx.derive_worktree()
    } else {
        ctx.worktree.clone()
    };
    if in_derive && !stalled_worktree.exists() {
        bail!(
            "the build stalled reconstructing a derived entry, but its worktree at {} is \
             gone -- the reconstruction it held cannot be resumed. Remove {} to abandon \
             this build and start it again with `fork-assembler build`.",
            stalled_worktree.display(),
            ctx.worktree.display()
        );
    }
    let unresolved = conflicted_files(&stalled_worktree)?;
    if !unresolved.is_empty() {
        bail!(
            "the {} worktree ({}) still has unresolved conflicts:\n  {}",
            if in_derive { "derive" } else { "build" },
            stalled_worktree.display(),
            unresolved.join("\n  ")
        );
    }
    let git_dir = git::git_dir(&stalled_worktree)?;

    if let Some(ds) = run.st.derive.take() {
        derive::resume(&mut run, ds)?;
    } else if let Some(pending) = run.st.pending.take() {
        run.resume_fixup(pending)?;
    } else if git_dir.join("MERGE_HEAD").exists() {
        if !run.resume_merge()? {
            return Ok(STOPPED);
        }
    } else if git_dir.join("MERGE_MSG").exists() || !staged_files(&ctx.worktree)?.is_empty() {
        run.resume_patch()?;
    }

    ctx.emit(Event::Resuming {
        index: run.st.next_index,
        total: ctx.manifest.entries.len(),
    });
    let previous = lock::load(&ctx.root)?;
    let resume_at = run.st.next_index;
    run.drive(resume_at, previous.as_ref(), false)
}
