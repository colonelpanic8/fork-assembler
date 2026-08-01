//! The build engine: assemble the stack from the lock's pins.
//!
//! `build` never moves an existing pin (`update` is the only verb that does);
//! entries with no pin yet get pinned from live refs on their first build.
//! The assembled branch is compiled output. `tree` (pre-provenance) is the
//! reproducibility invariant.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;

use crate::git;
use crate::lock::{self, DerivedResult, EntryResult, Lock, Prefix};
use crate::manifest::{self, Entry, Kind, Manifest, Parent};
use crate::rerere;
use crate::source;
use crate::state::{self, DeriveState, State};

pub const WORKTREE: &str = ".worktrees/build";

/// Where derived entries are reconstructed. A second worktree, because
/// reconstruction is a merge sequence of its own that must not disturb the
/// half-assembled stack it will eventually be merged into.
pub const DERIVE_WORKTREE: &str = ".worktrees/derive";

/// Exit code signalling "stopped for human resolution".
pub const STOPPED: i32 = 2;

fn short(oid: &str) -> &str {
    &oid[..12.min(oid.len())]
}

pub struct Ctx {
    pub root: PathBuf,
    pub manifest: Manifest,
    pub repo: PathBuf,
    pub worktree: PathBuf,
}

impl Ctx {
    pub fn open(root: &Path, allow_clone: bool) -> Result<Ctx> {
        let manifest = manifest::load(root)?;
        let repo = source::source_repo(root, &manifest, allow_clone)?;
        Ok(Ctx {
            root: root.to_path_buf(),
            worktree: root.join(WORKTREE),
            manifest,
            repo,
        })
    }

    pub fn derive_worktree(&self) -> PathBuf {
        self.root.join(DERIVE_WORKTREE)
    }
}

/// A private ref namespace so fetched heads never collide with user refs.
fn holding_ref(entry: &Entry) -> String {
    format!("refs/fork-fold/{}", manifest::sanitize_name(&entry.name))
}

/// Parents get their own namespace under the entry that declares them: two
/// entries may legitimately declare the same parent, and each reconstruction
/// is standalone, so the holding refs must not be shared between them.
fn parent_holding_ref(entry: &Entry, parent: &Parent) -> String {
    format!(
        "refs/fork-fold/parents/{}/{}",
        manifest::sanitize_name(&entry.name),
        manifest::sanitize_name(&parent.name)
    )
}

/// Where a reconstructed tip is parked. Nothing reads it back — the lock
/// records the OID — but it keeps the reconstruction reachable for the rest of
/// the build and leaves it inspectable afterwards, once the derive worktree is
/// gone.
fn derived_ref(entry: &Entry) -> String {
    format!(
        "refs/fork-fold/derived/{}",
        manifest::sanitize_name(&entry.name)
    )
}

fn fetch_into(ctx: &Ctx, remote: &str, spec: &str, holding: &str) -> Result<String> {
    git::out(&ctx.repo, &["fetch", remote, spec])?;
    git::out(&ctx.repo, &["rev-parse", holding])
}

pub fn fetch_entry(ctx: &Ctx, entry: &Entry) -> Result<String> {
    let holding = holding_ref(entry);
    match &entry.kind {
        Kind::Branch { remote, branch, .. } => fetch_into(
            ctx,
            remote,
            &format!("+refs/heads/{branch}:{holding}"),
            &holding,
        ),
        Kind::Pr { remote, number } => fetch_into(
            ctx,
            remote,
            &format!("+refs/pull/{number}/head:{holding}"),
            &holding,
        ),
        Kind::Patch { path } => patch_blob(&ctx.root, path),
    }
}

pub fn fetch_parent(ctx: &Ctx, entry: &Entry, parent: &Parent) -> Result<String> {
    let holding = parent_holding_ref(entry, parent);
    match &parent.kind {
        Kind::Branch { remote, branch, .. } => fetch_into(
            ctx,
            remote,
            &format!("+refs/heads/{branch}:{holding}"),
            &holding,
        ),
        Kind::Pr { remote, number } => fetch_into(
            ctx,
            remote,
            &format!("+refs/pull/{number}/head:{holding}"),
            &holding,
        ),
        // The manifest refuses a patch parent, so this arm is unreachable.
        Kind::Patch { path } => bail!("{}: parent {path:?} is a patch", entry.name),
    }
}

pub fn fetch_base(ctx: &Ctx) -> Result<String> {
    let remote = ctx.manifest.base.remote.clone();
    let ref_ = ctx.manifest.base.ref_.clone();
    let spec = format!("+refs/heads/{ref_}:refs/fork-fold/base");
    git::out(&ctx.repo, &["fetch", &remote, &spec])?;
    git::out(&ctx.repo, &["rev-parse", "refs/fork-fold/base"])
}

pub fn patch_blob(root: &Path, rel: &str) -> Result<String> {
    let path = root.join(rel);
    if !path.exists() {
        bail!("patch file {rel} does not exist");
    }
    git::out(root, &["hash-object", &path.to_string_lossy()])
}

/// Blob hash per entry carrying a coherence fixup, for lock snapshots.
/// `strict` fails on a missing file (what `build` wants); otherwise the entry
/// is left out of the map (what `status` wants, so it can still report).
pub fn fixup_blobs(
    root: &Path,
    entries: &[Entry],
    strict: bool,
) -> Result<BTreeMap<String, String>> {
    let mut blobs = BTreeMap::new();
    for entry in entries {
        let Some(rel) = &entry.fixup else { continue };
        match patch_blob(root, rel) {
            Ok(blob) => {
                blobs.insert(entry.name.clone(), blob);
            }
            Err(err) if strict => {
                return Err(err.context(format!("{}: coherence fixup", entry.name)))
            }
            Err(_) => {}
        }
    }
    Ok(blobs)
}

/// Resolve the pin for one entry, pinning from live refs when permitted.
fn ensure_pin(
    ctx: &Ctx,
    entry: &Entry,
    pins: &mut BTreeMap<String, String>,
    locked: bool,
) -> Result<String> {
    if let Some(pin) = pins.get(&entry.name) {
        let pin = pin.clone();
        match &entry.kind {
            Kind::Patch { path } => {
                let current = patch_blob(&ctx.root, path)?;
                if current != pin {
                    bail!(
                        "{}: patch file content changed since it was pinned; run `fork-fold update {}`",
                        entry.name,
                        entry.name
                    );
                }
            }
            _ => {
                if !git::has_commit(&ctx.repo, &pin) {
                    if locked {
                        bail!(
                            "{}: pinned OID {pin} is not present locally and --locked forbids fetching",
                            entry.name
                        );
                    }
                    fetch_entry(ctx, entry)?;
                    if !git::has_commit(&ctx.repo, &pin) {
                        bail!(
                            "{}: pinned OID {pin} is not reachable from its live ref; \
                             the branch moved on or was rewritten (fetch it manually or `fork-fold update {}`)",
                            entry.name,
                            entry.name
                        );
                    }
                }
            }
        }
        return Ok(pin);
    }
    if locked {
        bail!(
            "{}: no pin recorded and --locked forbids pinning; run `fork-fold build` or `update` first",
            entry.name
        );
    }
    let oid = fetch_entry(ctx, entry)?;
    println!("  pinned {} -> {}", entry.name, short(&oid));
    pins.insert(entry.name.clone(), oid.clone());
    Ok(oid)
}

/// Resolve every parent pin of one derived entry, pinning from live refs on
/// its first build. Parents obey the entry pin rule exactly: `build` may
/// establish one that has never been pinned, and moves none that has.
fn ensure_parent_pins(
    ctx: &Ctx,
    entry: &Entry,
    parent_pins: &mut BTreeMap<String, BTreeMap<String, String>>,
    locked: bool,
) -> Result<()> {
    let mut pins = parent_pins.get(&entry.name).cloned().unwrap_or_default();
    for parent in &entry.parents {
        match pins.get(&parent.name).cloned() {
            Some(pin) => {
                if !git::has_commit(&ctx.repo, &pin) {
                    if locked {
                        bail!(
                            "{}: parent {} is pinned at {pin}, which is not present locally, \
                             and --locked forbids fetching",
                            entry.name,
                            parent.name
                        );
                    }
                    fetch_parent(ctx, entry, parent)?;
                    if !git::has_commit(&ctx.repo, &pin) {
                        bail!(
                            "{}: parent {} is pinned at {pin}, which is not reachable from its \
                             live ref; the parent was rewritten (fetch it manually or \
                             `fork-fold update {}`)",
                            entry.name,
                            parent.name,
                            entry.name
                        );
                    }
                }
            }
            None => {
                if locked {
                    bail!(
                        "{}: parent {} has no pin recorded and --locked forbids pinning; \
                         run `fork-fold build` or `update {}` first",
                        entry.name,
                        parent.name,
                        entry.name
                    );
                }
                let oid = fetch_parent(ctx, entry, parent)?;
                println!(
                    "  pinned {}'s parent {} -> {}",
                    entry.name,
                    parent.name,
                    short(&oid)
                );
                pins.insert(parent.name.clone(), oid);
            }
        }
    }
    // Parents that the manifest no longer declares stop being facts about this
    // entry the moment the declaration goes.
    pins.retain(|name, _| entry.parents.iter().any(|p| &p.name == name));
    parent_pins.insert(entry.name.clone(), pins);
    Ok(())
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

fn merge_entry(ctx: &Ctx, entry: &Entry, oid: &str) -> Result<bool> {
    let message = format!("fork-fold: merge {}", entry.name);
    let out = git::raw(
        &ctx.worktree,
        &rerere::with_cfg(&["merge", "--no-ff", "--no-edit", "-m", &message, oid]),
    )?;
    Ok(out.status.success())
}

/// git config for the replay phase: the rerere pair machinery plus an editor
/// that exits successfully without opening anything. `cherry-pick` and its
/// `--continue` want an editor by default, and a build has no terminal to give
/// them one.
fn replay_cfg<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut cfg = vec!["-c", "core.editor=true"];
    cfg.extend(rerere::with_cfg(args));
    cfg
}

fn is_ancestor(repo: &Path, commit: &str, of: &str) -> bool {
    git::ok(repo, &["merge-base", "--is-ancestor", commit, of])
}

/// Which rule established a derived entry's anchor. Printed on every
/// resolution: the anchor decides which commits are replayed as the entry's
/// own, so an operator who cannot see how it was chosen cannot audit the
/// boundary — and a wrong boundary silently duplicates or drops work.
pub enum AnchorRule {
    /// The last reconstruction's parent merge is an ancestor of the new pin:
    /// the operator pushed the reconstructed tip, and everything the PR has
    /// gained since (review commits, say) is the entry's own work.
    PushedReconstruction,
    /// The recorded anchor is still an ancestor of the pin, so it still marks
    /// the same boundary.
    Kept,
    /// Walked the pin's first-parent chain to the first commit that is a merge
    /// or is already contained in the base or a parent.
    Detected,
}

pub struct Anchor {
    pub oid: String,
    pub rule: AnchorRule,
}

impl Anchor {
    /// The audit sentence: which rule fired and what it means.
    pub fn describe(&self) -> &'static str {
        match self.rule {
            AnchorRule::PushedReconstruction => {
                "the last reconstruction's parent merge is an ancestor of the pin \
                 -- the reconstructed tip was pushed, so everything above it is the \
                 entry's own work"
            }
            AnchorRule::Kept => "unchanged: the recorded anchor is still an ancestor of the pin",
            AnchorRule::Detected => {
                "detected: the first-parent walk stopped at the first merge or \
                 already-contained commit"
            }
        }
    }
}

/// The commit in a derived entry's history after which its own commits start.
///
/// Everything about a derived entry depends on getting this right, and the
/// obvious alternatives get it wrong. `git cherry` and `rev-list C ^A ^B` both
/// answer "which commits are C's own?" by comparing against the parents' *live
/// tips*, which is exactly the comparison that breaks when a parent is rebased:
/// the parent commits embedded in C's history stop matching anything reachable
/// from the new tips, and get replayed as C's own work — duplicating the old
/// version of the parent on top of the new one. Anchoring on a commit inside
/// C's own history instead makes the delta `<pin> ^<anchor>`, which no movement
/// of a parent can perturb.
///
/// The rules are tried in order and the winner is reported, because the two
/// cheap rules exist to preserve a boundary that was already established and
/// only the last one guesses.
pub fn resolve_anchor(
    repo: &Path,
    entry: &Entry,
    pin: &str,
    base: Option<&str>,
    parent_pins: &BTreeMap<String, String>,
    previous_anchor: Option<&str>,
    previous_base_tip: Option<&str>,
) -> Result<Anchor> {
    if let Some(base_tip) = previous_base_tip {
        if git::has_commit(repo, base_tip) && is_ancestor(repo, base_tip, pin) {
            return Ok(Anchor {
                oid: base_tip.to_string(),
                rule: AnchorRule::PushedReconstruction,
            });
        }
    }
    if let Some(anchor) = previous_anchor {
        if git::has_commit(repo, anchor) && is_ancestor(repo, anchor, pin) {
            return Ok(Anchor {
                oid: anchor.to_string(),
                rule: AnchorRule::Kept,
            });
        }
    }
    Ok(Anchor {
        oid: detect_anchor(repo, entry, pin, base, parent_pins)?,
        rule: AnchorRule::Detected,
    })
}

/// Walk the pin's first-parent chain down to the first commit that is a merge,
/// or that the base or some parent already contains. That commit is the
/// boundary: above it is work this entry added, below it is history it merged
/// or inherited. The tip itself qualifying means the entry is a pure merge of
/// its parents and its delta is empty.
fn detect_anchor(
    repo: &Path,
    entry: &Entry,
    pin: &str,
    base: Option<&str>,
    parent_pins: &BTreeMap<String, String>,
) -> Result<String> {
    // One rev-list answers "contained?" for the whole chain at once: a commit
    // it prints is reachable from the pin along first parents and from none of
    // the boundaries.
    let mut args = vec!["rev-list".to_string(), "--first-parent".into(), pin.into()];
    for boundary in base
        .into_iter()
        .chain(parent_pins.values().map(String::as_str))
    {
        args.push(format!("^{boundary}"));
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let uncontained: BTreeSet<String> =
        git::out(repo, &refs)?.lines().map(str::to_string).collect();

    let log = git::out(repo, &["log", "--first-parent", "--format=%H %P", pin])?;
    for line in log.lines() {
        let (oid, parents) = line.split_once(' ').unwrap_or((line, ""));
        let is_merge = parents.split_whitespace().count() > 1;
        if is_merge || !uncontained.contains(oid) {
            return Ok(oid.to_string());
        }
    }
    bail!(
        "{}: cannot find the anchor for this derived entry. Walking {}'s first-parent \
         chain reached the root without meeting a merge commit or a commit already \
         contained in the base or in a parent, so there is no boundary between the \
         entry's own work and what it inherited.\n\
         A derived entry must MERGE its parents in; one that cherry-picks or rebases \
         them onto itself keeps no record of where they end. Either rebuild the branch \
         as merges of {}, or drop its `parents` declaration and carry it as an \
         ordinary entry.",
        entry.name,
        short(pin),
        entry
            .parents
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>()
            .join(", "),
    )
}

/// The entry's own commits, oldest first. Merges are excluded: replaying one
/// commit at a time cannot reproduce a merge, and a derived entry whose own
/// work contains merges is outside what this reconstructs (see DESIGN.md).
fn delta_commits(repo: &Path, pin: &str, anchor: &str) -> Result<Vec<String>> {
    let excluded = format!("^{anchor}");
    let out = git::out(
        repo,
        &["rev-list", "--reverse", "--no-merges", pin, &excluded],
    )?;
    Ok(out.lines().map(str::to_string).collect())
}

/// Persist the reconstruction's progress. Every step, like the entry loop:
/// resuming a merge sequence or a cherry-pick sequence at a stale index
/// re-applies work that is already in.
fn persist_derive(ctx: &Ctx, st: &mut State, ds: &DeriveState) -> Result<()> {
    st.derive = Some(ds.clone());
    state::write(&ctx.worktree, st)
}

fn stop_in_derive(ctx: &Ctx, entry: &Entry, label: &str, doing: &str, unresolved: &[String]) {
    println!(
        "\n  {label} CONFLICT {doing} in {} file(s):",
        unresolved.len()
    );
    for file in unresolved {
        println!("      {file}");
    }
    println!(
        "\n  Resolve in the DERIVE worktree: {}",
        ctx.derive_worktree().display()
    );
    println!(
        "  That worktree holds {}'s reconstruction, not the assembled stack.",
        entry.name
    );
    println!("  Stage with `git add`, then: fork-fold continue");
}

/// Reconstruct a derived entry: re-merge its parents onto the pinned base,
/// then replay the entry's own commits on top. The result is merged into the
/// stack in place of the entry's pin, which is stale by construction — it was
/// built against whatever its parents were at the time.
///
/// Returns None when the reconstruction stopped for the human, having already
/// persisted where it got to.
fn reconstruct(
    ctx: &Ctx,
    st: &mut State,
    index: usize,
    entry: &Entry,
    label: &str,
    pin: &str,
) -> Result<Option<DerivedResult>> {
    let derive = ctx.derive_worktree();
    let mut ds = match st.derive.take() {
        Some(ds) if ds.entry_index == index => ds,
        _ => {
            prepare_worktree(ctx, &derive, &st.base)?;
            println!(
                "  {label} reconstructing from base {} in {}",
                short(&st.base),
                derive.display()
            );
            DeriveState {
                entry_index: index,
                next_parent: 0,
                base_tip: None,
                delta: Vec::new(),
                next_pick: 0,
            }
        }
    };
    persist_derive(ctx, st, &ds)?;

    while ds.next_parent < entry.parents.len() {
        let parent = &entry.parents[ds.next_parent];
        let parent_pin = st
            .parent_pins
            .get(&entry.name)
            .and_then(|pins| pins.get(&parent.name))
            .cloned()
            .with_context(|| {
                format!(
                    "{}: parent {} pin vanished mid-build",
                    entry.name, parent.name
                )
            })?;
        let head = git::out(&derive, &["rev-parse", "HEAD"])?;
        if is_ancestor(&ctx.repo, &parent_pin, &head) {
            // Either the base already contains this parent, or an earlier
            // parent does. Merging it would add an empty merge commit and say
            // nothing; the operator wants to hear it, though, because a parent
            // that is permanently absorbed no longer belongs in the list.
            println!(
                "  {label} parent {} ABSORBED -- already in the reconstruction",
                parent.name
            );
        } else {
            let message = format!(
                "fork-fold: merge parent {} (for {})",
                parent.name, entry.name
            );
            let out = git::raw(
                &derive,
                &rerere::with_cfg(&["merge", "--no-ff", "--no-edit", "-m", &message, &parent_pin]),
            )?;
            if !out.status.success() {
                st.conflicts += 1;
                let unresolved = conflicted_files(&derive)?;
                if unresolved.is_empty() {
                    git::out(&derive, &rerere::with_cfg(&["commit", "--no-edit"]))?;
                    println!(
                        "  {label} parent {} merged; auto-resolved from tracked rerere pairs",
                        parent.name
                    );
                } else {
                    persist_derive(ctx, st, &ds)?;
                    stop_in_derive(
                        ctx,
                        entry,
                        label,
                        &format!("merging parent {}", parent.name),
                        &unresolved,
                    );
                    return Ok(None);
                }
            } else {
                println!(
                    "  {label} parent {} merged {}",
                    parent.name,
                    short(&parent_pin)
                );
            }
        }
        ds.next_parent += 1;
        persist_derive(ctx, st, &ds)?;
    }

    if ds.base_tip.is_none() {
        let base_tip = git::out(&derive, &["rev-parse", "HEAD"])?;
        let anchor = ensure_anchor(ctx, st, entry, label, pin)?;
        let delta = delta_commits(&ctx.repo, pin, &anchor)?;
        match delta.len() {
            0 => println!("  {label} delta: none -- a pure merge of its parents"),
            n => println!("  {label} delta: {n} commit(s) of its own after the anchor"),
        }
        ds.base_tip = Some(base_tip);
        ds.delta = delta;
        persist_derive(ctx, st, &ds)?;
    }

    while ds.next_pick < ds.delta.len() {
        let commit = ds.delta[ds.next_pick].clone();
        let subject = git::out(&ctx.repo, &["log", "-1", "--format=%s", &commit])?;
        let out = git::raw(&derive, &replay_cfg(&["cherry-pick", &commit]))?;
        if !out.status.success() {
            let unresolved = conflicted_files(&derive)?;
            if !unresolved.is_empty() {
                st.conflicts += 1;
                persist_derive(ctx, st, &ds)?;
                stop_in_derive(
                    ctx,
                    entry,
                    label,
                    &format!("replaying {} ({subject})", short(&commit)),
                    &unresolved,
                );
                return Ok(None);
            }
            // Nothing conflicted, so either rerere replayed every hunk and
            // staged the result, or the commit's content is already present
            // and there is nothing left to commit at all.
            if git::ok(&derive, &["diff", "--cached", "--quiet", "HEAD"]) {
                git::out(&derive, &replay_cfg(&["cherry-pick", "--skip"]))?;
                println!(
                    "  {label} replayed {} ({subject}): EMPTY -- already present",
                    short(&commit)
                );
            } else {
                st.conflicts += 1;
                git::out(&derive, &replay_cfg(&["cherry-pick", "--continue"]))?;
                println!(
                    "  {label} replayed {} ({subject}); auto-resolved from tracked rerere pairs",
                    short(&commit)
                );
            }
        } else {
            println!("  {label} replayed {} ({subject})", short(&commit));
        }
        ds.next_pick += 1;
        persist_derive(ctx, st, &ds)?;
    }

    let tip = git::out(&derive, &["rev-parse", "HEAD"])?;
    let base_tip = ds
        .base_tip
        .clone()
        .context("reconstruction finished without recording its parent merge")?;
    git::out(&ctx.repo, &["update-ref", &derived_ref(entry), &tip])?;
    let derived = DerivedResult { base_tip, tip };
    st.derive = None;
    st.derived.insert(entry.name.clone(), derived.clone());
    state::write(&ctx.worktree, st)?;
    Ok(Some(derived))
}

/// The anchor this build must replay from: whatever is already pinned, or —
/// on a first build, where nothing has established one yet — the rules in
/// `resolve_anchor`. A build never re-resolves a pinned anchor, for the reason
/// it never moves a pin: what a build replays would then depend on when it ran.
fn ensure_anchor(
    ctx: &Ctx,
    st: &mut State,
    entry: &Entry,
    label: &str,
    pin: &str,
) -> Result<String> {
    if let Some(anchor) = st.anchors.get(&entry.name) {
        return Ok(anchor.clone());
    }
    let previous_base_tip = lock::load(&ctx.root)?
        .and_then(|l| l.build)
        .and_then(|b| b.results.into_iter().find(|r| r.name == entry.name))
        .and_then(|r| r.derived)
        .map(|d| d.base_tip);
    let no_parents = BTreeMap::new();
    let anchor = resolve_anchor(
        &ctx.repo,
        entry,
        pin,
        Some(&st.base),
        st.parent_pins.get(&entry.name).unwrap_or(&no_parents),
        None,
        previous_base_tip.as_deref(),
    )?;
    println!(
        "  {label} anchor {} -- {}",
        short(&anchor.oid),
        anchor.describe()
    );
    st.anchors.insert(entry.name.clone(), anchor.oid.clone());
    state::write(&ctx.worktree, st)?;
    Ok(anchor.oid)
}

/// Apply a tracked patch file and commit it as `message`. `Some(outcome)` =
/// applied or already applied; `None` = failed, conflict left for the human.
/// Shared by patch entries and coherence fixups.
fn apply_patch_file(ctx: &Ctx, rel: &str, message: &str) -> Result<Option<&'static str>> {
    let patch = ctx.root.join(rel);
    let patch_str = patch.to_string_lossy().to_string();

    // "Reverse-applies" alone does not mean "already applied": a patch that
    // DELETES a duplicated block reverse-applies against the surviving copy,
    // because the copies are byte-identical and context matching cannot tell
    // them apart. Only trust the reverse check when the patch also cannot be
    // applied forwards.
    let reverse = git::raw(
        &ctx.worktree,
        &["apply", "--reverse", "--check", &patch_str],
    )?;
    let forward = git::raw(&ctx.worktree, &["apply", "--check", &patch_str])?;
    if reverse.status.success() && !forward.status.success() {
        return Ok(Some("already applied"));
    }

    let applied = git::raw(&ctx.worktree, &["apply", "--3way", &patch_str])?;
    if !applied.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&applied.stderr).trim_end());
        return Ok(None);
    }
    git::out(&ctx.worktree, &["add", "-A"])?;
    if git::out(&ctx.worktree, &["diff", "--cached", "--name-only"])?.is_empty() {
        return Ok(Some("already applied"));
    }
    git::out(&ctx.worktree, &["commit", "-q", "-m", message])?;
    Ok(Some("applied"))
}

/// Complete an entry's step: apply its coherence fixup, if any, then record
/// the result and advance. Applying the fixup HERE — inside the entry's step,
/// not as a later standalone entry — is what makes every entry boundary a
/// coherent tree.
///
/// Returns false when the fixup needs a human. The merge is already committed
/// at that point, so the merge's result is held in `state.pending`: its
/// presence is how `continue` knows to resume at the fixup rather than
/// re-running the merge.
fn finish_entry(
    ctx: &Ctx,
    st: &mut State,
    index: usize,
    entry: &Entry,
    label: &str,
    mut result: EntryResult,
) -> Result<bool> {
    if let Some(rel) = &entry.fixup {
        let blob = patch_blob(&ctx.root, rel)?;
        let message = format!("fork-fold: fixup {}", entry.name);
        match apply_patch_file(ctx, rel, &message)? {
            Some(outcome) => {
                println!("  {label} fixup {rel}: {outcome}");
                result.fixup = Some(blob);
            }
            None => {
                st.pending = Some(result);
                st.next_index = index;
                state::write(&ctx.worktree, st)?;
                println!("\n  {label} fixup {rel} FAILED to apply");
                println!("  The merge is committed; only the fixup is outstanding.");
                println!("  Resolve the markers in: {}", ctx.worktree.display());
                println!("  Then re-capture the corrected fixup and rebuild:");
                println!("      fork-fold fixup {} {rel} --capture", entry.name);
                println!("      fork-fold build");
                println!("  Or commit this resolution once, leaving {rel} stale:");
                println!("      git add -A && fork-fold continue");
                return Ok(false);
            }
        }
    }
    st.results.push(result);
    st.pending = None;
    st.next_index = index + 1;
    state::write(&ctx.worktree, st)?;
    Ok(true)
}

/// The core loop: process entries[start..], persisting state after each.
/// Returns Some(exit_code) when stopped for the human, None when complete.
fn run_entries(ctx: &Ctx, st: &mut State, start: usize) -> Result<Option<i32>> {
    let entries = &ctx.manifest.entries;
    let total = entries.len();
    for (index, entry) in entries.iter().enumerate().skip(start) {
        let label = format!("[{:2}/{total}] {:<24}", index + 1, entry.name);
        let oid = st
            .pins
            .get(&entry.name)
            .cloned()
            .with_context(|| format!("{}: pin vanished mid-build", entry.name))?;

        if let Kind::Patch { path } = &entry.kind {
            let rel = path.clone();
            let message = format!("fork-fold: {}", entry.name);
            match apply_patch_file(ctx, &rel, &message)? {
                Some(outcome) => {
                    println!("  {label} {outcome}");
                    let result = EntryResult {
                        name: entry.name.clone(),
                        oid,
                        status: "applied".into(),
                        conflicted: false,
                        resolution: None,
                        fixup: None,
                        derived: None,
                    };
                    // Patch entries cannot carry a fixup (manifest rejects
                    // it), so this only records and advances.
                    finish_entry(ctx, st, index, entry, &label, result)?;
                }
                None => {
                    st.next_index = index;
                    state::write(&ctx.worktree, st)?;
                    println!("\n  {label} patch FAILED to apply");
                    println!("  Resolve in: {}", ctx.worktree.display());
                    println!("  Then: fork-fold continue");
                    return Ok(Some(STOPPED));
                }
            }
            continue;
        }

        if git::ok(&ctx.repo, &["merge-base", "--is-ancestor", &oid, &st.base]) {
            println!("  {label} ABSORBED upstream -- drop candidate");
            // A derived entry stops here too, before any reconstruction: the
            // base already contains the whole thing, parents and own commits
            // alike, so there is nothing left to rebuild it out of.
            // The fixup still runs: this entry's content reaching the tree via
            // the base rather than via a merge does not mean the incoherence
            // it repaired went away. If it did, the fixup reports "already
            // applied"; if it did not, it applies as usual.
            let result = EntryResult {
                name: entry.name.clone(),
                oid,
                status: "absorbed".into(),
                conflicted: false,
                resolution: None,
                fixup: None,
                derived: None,
            };
            if !finish_entry(ctx, st, index, entry, &label, result)? {
                return Ok(Some(STOPPED));
            }
            continue;
        }

        // A derived entry's pin is stale by construction: it was built against
        // whatever its parents were then. What gets merged is the
        // reconstruction; what gets recorded is still the pin.
        let derived = if entry.parents.is_empty() {
            None
        } else {
            match reconstruct(ctx, st, index, entry, &label, &oid)? {
                Some(derived) => Some(derived),
                None => return Ok(Some(STOPPED)),
            }
        };
        let merging = derived.as_ref().map_or(oid.as_str(), |d| d.tip.as_str());

        let before = git::out(&ctx.worktree, &["rev-parse", "HEAD^{tree}"])?;
        let clean = merge_entry(ctx, entry, merging)?;

        if !clean {
            st.conflicts += 1;
            let unresolved = conflicted_files(&ctx.worktree)?;
            if unresolved.is_empty() {
                // rerere recognized every conflict hunk and staged the
                // recorded resolutions (autoUpdate); commit and continue.
                let hashes: Vec<String> = rerere::merge_rr(&ctx.worktree)?
                    .into_iter()
                    .map(|(hash, _)| hash)
                    .collect();
                git::out(&ctx.worktree, &rerere::with_cfg(&["commit", "--no-edit"]))?;
                println!("  {label} auto-resolved from tracked rerere pairs");
                let result = EntryResult {
                    name: entry.name.clone(),
                    oid,
                    status: "merged".into(),
                    conflicted: true,
                    resolution: Some(rerere::label(&hashes)),
                    fixup: None,
                    derived,
                };
                let done = finish_entry(ctx, st, index, entry, &label, result)?;
                clean_derive(ctx, entry, done);
                if !done {
                    return Ok(Some(STOPPED));
                }
                continue;
            }
            st.next_index = index;
            state::write(&ctx.worktree, st)?;
            println!("\n  {label} CONFLICT in {} file(s):", unresolved.len());
            for file in &unresolved {
                println!("      {file}");
            }
            println!("\n  Resolve in: {}", ctx.worktree.display());
            println!("  Stage with `git add`, then: fork-fold continue");
            return Ok(Some(STOPPED));
        }

        let after = git::out(&ctx.worktree, &["rev-parse", "HEAD^{tree}"])?;
        let status = if before == after {
            println!("  {label} EMPTY -- merge changed nothing, drop candidate");
            "empty"
        } else {
            // Naming the reconstruction is the point: the OID that just landed
            // in the stack is not the pin the lock records for this entry, and
            // an operator reading the log needs to know which is which.
            let what = if derived.is_some() {
                "merged reconstruction"
            } else {
                "merged"
            };
            println!("  {label} {what} {}", short(merging));
            "merged"
        };
        let result = EntryResult {
            name: entry.name.clone(),
            oid,
            status: status.into(),
            conflicted: false,
            resolution: None,
            fixup: None,
            derived,
        };
        let done = finish_entry(ctx, st, index, entry, &label, result)?;
        clean_derive(ctx, entry, done);
        if !done {
            return Ok(Some(STOPPED));
        }
    }
    Ok(None)
}

/// Drop the reconstruction worktree once its entry's step has completed.
///
/// It survives an unfinished step on purpose: when a build stops on the stack
/// merge or on a fixup, the reconstruction that produced the conflicting side
/// is exactly what the operator needs to read next to it.
fn clean_derive(ctx: &Ctx, entry: &Entry, step_completed: bool) {
    if step_completed && !entry.parents.is_empty() {
        remove_worktree(ctx, &ctx.derive_worktree());
    }
}

fn provenance_json(ctx: &Ctx, st: &State, base: &str) -> Result<serde_json::Value> {
    use serde_json::json;
    let m = &ctx.manifest;
    let subject = git::out(&ctx.repo, &["log", "-1", "--format=%s", base])?;
    let date = git::out(&ctx.repo, &["log", "-1", "--format=%cI", base])?;

    let results: BTreeMap<&str, &EntryResult> =
        st.results.iter().map(|r| (r.name.as_str(), r)).collect();
    let entries: Vec<serde_json::Value> = m
        .entries
        .iter()
        .map(|entry| {
            let result = results.get(entry.name.as_str());
            let mut record = json!({
                "label": entry.name,
                "kind": entry.kind.kind_str(),
                "status": result.map(|r| r.status.as_str()).unwrap_or("unknown"),
                "commit": result.map(|r| r.oid.as_str()).unwrap_or(""),
            });
            let obj = record.as_object_mut().expect("record is an object");
            if let Some(pr) = entry.pr_number() {
                obj.insert("pr".into(), json!(pr));
            }
            if let Kind::Branch { branch, .. } = &entry.kind {
                obj.insert("branch".into(), json!(branch));
            }
            if let Some(fixup) = &entry.fixup {
                obj.insert("fixup".into(), json!(fixup));
            }
            // A derived entry's `commit` is its pin, which by itself explains
            // none of the tree it contributed. The parents and the two
            // reconstruction commits are what make that tree accountable.
            if !entry.parents.is_empty() {
                let pins = st.parent_pins.get(&entry.name);
                obj.insert(
                    "parents".into(),
                    json!(entry
                        .parents
                        .iter()
                        .map(|parent| json!({
                            "label": parent.name,
                            "source": parent.source(),
                            "commit": pins
                                .and_then(|pins| pins.get(&parent.name))
                                .cloned()
                                .unwrap_or_default(),
                        }))
                        .collect::<Vec<_>>()),
                );
                if let Some(derived) = result.and_then(|r| r.derived.as_ref()) {
                    obj.insert(
                        "derived".into(),
                        json!({ "baseTip": derived.base_tip, "tip": derived.tip }),
                    );
                }
            }
            if let Some(summary) = &entry.summary {
                obj.insert("summary".into(), json!(summary));
            }
            if let Some(note) = &entry.note {
                obj.insert("note".into(), json!(note.trim()));
            }
            record
        })
        .collect();

    let mut top = json!({
        "schemaVersion": 1,
        "manifest": manifest::FILE,
        "upstream": {
            "remote": m.remote_url(&m.base.remote)?,
            "ref": m.base.ref_,
            "commit": base,
            "subject": subject,
            "date": date,
        },
        "entries": entries,
    });
    if let Some(publish) = &m.publish {
        let remote_url = publish
            .remote
            .as_deref()
            .and_then(|name| m.remotes.get(name).cloned());
        top.as_object_mut().expect("top is an object").insert(
            "fork".into(),
            json!({
                "remote": remote_url,
                "branch": publish.branch,
            }),
        );
    }
    Ok(top)
}

/// Finish a completed run: provenance commit, lock write, reporting.
fn finalize(ctx: &Ctx, st: &State, previous: Option<&Lock>, write_lock: bool) -> Result<()> {
    let pre_provenance = git::out(&ctx.worktree, &["rev-parse", "HEAD"])?;
    // The content tree BEFORE the provenance commit: `tree` has to keep
    // meaning "what the topics and base produced", or every rebuild would
    // look changed and the "tree unchanged" signal would die.
    let tree = git::out(&ctx.worktree, &["rev-parse", "HEAD^{tree}"])?;

    let head = if let Some(file) = &ctx.manifest.provenance_file {
        let provenance = provenance_json(ctx, st, &st.base)?;
        let body = serde_json::to_string_pretty(&provenance)? + "\n";
        std::fs::write(ctx.worktree.join(file), body)?;
        git::out(&ctx.worktree, &["add", file])?;
        if !git::out(&ctx.worktree, &["status", "--porcelain", "--", file])?.is_empty() {
            git::out(
                &ctx.worktree,
                &["commit", "-q", "-m", "fork-fold: record build provenance"],
            )?;
        }
        git::out(&ctx.worktree, &["rev-parse", "HEAD"])?
    } else {
        pre_provenance.clone()
    };
    let built_tree = git::out(&ctx.worktree, &["rev-parse", "HEAD^{tree}"])?;

    let previous_tree = previous
        .and_then(|l| l.build.as_ref())
        .map(|b| b.tree.clone());

    println!("\ntree:   {tree}");
    println!("commit: {head}");
    println!("conflicts this run: {}", st.conflicts);
    match &previous_tree {
        Some(prev) if *prev == tree => {
            println!("tree UNCHANGED from previous lock -- nothing downstream needs a bump")
        }
        Some(prev) => println!("tree CHANGED (was {prev})"),
        None => {}
    }

    if write_lock {
        let mut lock = previous.cloned().unwrap_or_default();
        lock.pins.base = Some(st.base.clone());
        lock.pins.entries = st.pins.clone();
        // A first build establishes a derived entry's parent pins and anchor;
        // every later one carries forward what it consumed.
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
            previous_tree: previous_tree.clone(),
            tree_changed: previous_tree.as_deref() != Some(tree.as_str()),
            conflicts: st.conflicts,
            manifest_entries,
            results: st.results.clone(),
        });
        lock::save(&ctx.root, &lock)?;
        println!("wrote {}", lock::FILE);
    } else {
        let expected = previous.and_then(|l| l.build.as_ref()).map(|b| &b.tree);
        match expected {
            Some(expected) if *expected == tree => {
                println!("verified: reproduced the lock's tree exactly")
            }
            Some(expected) => {
                bail!("reproduction FAILED: built tree {tree} but the lock records {expected}")
            }
            None => println!("(no lock to verify against; lock not written in --locked mode)"),
        }
    }
    state::clear(&ctx.worktree)?;
    Ok(())
}

pub fn build(root: &Path, locked: bool) -> Result<i32> {
    let ctx = Ctx::open(root, !locked)?;
    if let Some(st) = state::read(&ctx.worktree).unwrap_or(None) {
        let _ = st;
        bail!(
            "a build is already in progress in {}; finish it with `fork-fold continue` \
             (or remove the worktree to abandon it)",
            ctx.worktree.display()
        );
    }

    let previous = lock::load(&ctx.root)?;
    let mut pins = previous
        .as_ref()
        .map(|l| l.pins.clone())
        .unwrap_or_default();

    let base = match previous.as_ref().and_then(|l| l.pins.base.clone()) {
        Some(base) => {
            if !git::has_commit(&ctx.repo, &base) {
                if locked {
                    bail!(
                        "pinned base {base} is not present locally and --locked forbids fetching"
                    );
                }
                fetch_base(&ctx)?;
                if !git::has_commit(&ctx.repo, &base) {
                    bail!("pinned base {base} is not reachable from the live base ref");
                }
            }
            base
        }
        None => {
            if locked {
                bail!("no base pin recorded and --locked forbids pinning");
            }
            let oid = fetch_base(&ctx)?;
            println!("  pinned base -> {}", &oid[..12.min(oid.len())]);
            oid
        }
    };

    for entry in &ctx.manifest.entries {
        ensure_pin(&ctx, entry, &mut pins.entries, locked)?;
        if !entry.parents.is_empty() {
            ensure_parent_pins(&ctx, entry, &mut pins.parents, locked)?;
        }
    }
    // Parent pins and anchors for entries the manifest no longer carries are
    // not facts about anything; carrying them forward would leave the lock
    // asserting relationships that nothing declares.
    pins.parents
        .retain(|name, _| ctx.manifest.entries.iter().any(|e| &e.name == name));
    pins.anchors
        .retain(|name, _| ctx.manifest.entries.iter().any(|e| &e.name == name));

    // Decide full rebuild vs incremental extension vs up-to-date. Fixup blobs
    // ride in the snapshot, so editing one invalidates from its entry exactly
    // as repinning that entry would.
    let fixups = fixup_blobs(&ctx.root, &ctx.manifest.entries, true)?;
    let snapshot = lock::snapshot(&ctx.manifest.entries, &pins, &fixups);
    let mut start = 0usize;
    let mut extended_from = None;
    let mut results: Vec<EntryResult> = Vec::new();
    let relation = previous
        .as_ref()
        .map(|l| lock::prefix_relation(l, &snapshot, &base))
        .unwrap_or(Prefix::NoBuild);

    let start_commit = match relation {
        Prefix::Exact if !locked => {
            let build = previous
                .as_ref()
                .and_then(|l| l.build.as_ref())
                .expect("Exact implies a build");
            if git::has_commit(&ctx.repo, &build.pre_provenance_commit) {
                println!("up to date: tree {}", build.tree);
                return Ok(0);
            }
            base.clone()
        }
        Prefix::Extension(prefix_len) if !locked => {
            let build = previous
                .as_ref()
                .and_then(|l| l.build.as_ref())
                .expect("Extension implies a build");
            if git::has_commit(&ctx.repo, &build.pre_provenance_commit) {
                println!(
                    "extending the locked build ({} new entr{})",
                    snapshot.len() - prefix_len,
                    if snapshot.len() - prefix_len == 1 {
                        "y"
                    } else {
                        "ies"
                    }
                );
                start = prefix_len;
                results = build.results.clone();
                extended_from = Some(build.pre_provenance_commit.clone());
                build.pre_provenance_commit.clone()
            } else {
                base.clone()
            }
        }
        _ => base.clone(),
    };
    if start == 0 {
        println!("building from base {}", short(&base));
        results.clear();
    }

    prepare_worktree(&ctx, &ctx.worktree, &start_commit)?;
    // Any reconstruction worktree still here belongs to a build that is over:
    // this one refused to start while state existed.
    remove_worktree(&ctx, &ctx.derive_worktree());
    // One seeding covers both worktrees: rr-cache lives in the source repo's
    // common git dir, which every worktree of it shares.
    let seeded = rerere::seed(&ctx.root, &ctx.worktree)?;
    if seeded > 0 {
        println!("seeded {seeded} tracked rerere pair(s)");
    }
    let mut st = State {
        next_index: start,
        base: base.clone(),
        pins: pins.entries,
        parent_pins: pins.parents,
        anchors: pins.anchors,
        results,
        conflicts: 0,
        extended_from,
        pending: None,
        derive: None,
        derived: BTreeMap::new(),
    };
    state::write(&ctx.worktree, &st)?;

    match run_entries(&ctx, &mut st, start)? {
        Some(code) => Ok(code),
        None => {
            finalize(&ctx, &st, previous.as_ref(), !locked)?;
            Ok(0)
        }
    }
}

/// The worktree's own git dir, absolute — where the in-flight merge or
/// cherry-pick records itself.
fn git_dir(worktree: &Path) -> Result<PathBuf> {
    let dir = PathBuf::from(git::out(worktree, &["rev-parse", "--git-dir"])?);
    Ok(if dir.is_absolute() {
        dir
    } else {
        worktree.join(dir)
    })
}

/// What `continue` says about a resolution it just committed. Harvesting is
/// what makes a resolution durable; one rerere could not capture leaves the
/// next rebuild to stop in exactly the same place, and the operator should
/// hear that now rather than discover it then.
fn report_harvest(label: &str, what: &str, harvested: &[String]) {
    if harvested.is_empty() {
        println!(
            "  {label} {what}; WARNING: no rerere pair captured \
             (unrecognizable conflict) -- a rebuild will stop here again"
        );
    } else {
        println!(
            "  {label} {what}; harvested {} pair(s) into {}",
            harvested.len(),
            rerere::DIR,
        );
    }
}

pub fn cont(root: &Path) -> Result<i32> {
    let ctx = Ctx::open(root, false).or_else(|_| Ctx::open(root, true))?;
    let Some(mut st) = state::read(&ctx.worktree)? else {
        bail!("no in-progress build found; run `fork-fold build`");
    };

    // A reconstruction in flight owns the conflict: the build worktree is
    // merely paused behind it, and resolving there would repair nothing.
    let in_derive = st.derive.is_some();
    let stalled_worktree = if in_derive {
        ctx.derive_worktree()
    } else {
        ctx.worktree.clone()
    };
    if in_derive && !stalled_worktree.exists() {
        bail!(
            "the build stalled reconstructing a derived entry, but its worktree at {} is \
             gone -- the reconstruction it held cannot be resumed. Remove {} to abandon \
             this build and start it again with `fork-fold build`.",
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
    let git_dir = git_dir(&stalled_worktree)?;

    let stalled = st.next_index;
    let total = ctx.manifest.entries.len();
    let label = |index: usize, name: &str| format!("[{:2}/{total}] {name:<24}", index + 1);

    if let Some(mut ds) = st.derive.clone() {
        // The stall is inside a reconstruction. Finish whatever git has open
        // in the derive worktree and advance that sequence's index; the entry
        // loop below then re-enters the reconstruction where it left off,
        // merges the result into the stack, and carries on.
        let entry = &ctx.manifest.entries[ds.entry_index];
        let label = label(ds.entry_index, &entry.name);
        if git_dir.join("MERGE_HEAD").exists() {
            // Read MERGE_RR before committing: the rerere-enabled commit
            // records the postimages and clears it.
            let merge_rr = rerere::merge_rr(&stalled_worktree)?;
            git::out(
                &stalled_worktree,
                &rerere::with_cfg(&["commit", "--no-edit"]),
            )?;
            let harvested = rerere::harvest(&ctx.root, &stalled_worktree)?;
            // Attributed to the entry, not the parent: the entry is what the
            // manifest carries and what a later build will replay this for.
            rerere::index_add(&ctx.root, &entry.name, &harvested, &merge_rr)?;
            let what = match entry.parents.get(ds.next_parent) {
                Some(parent) => format!("parent {} merged", parent.name),
                None => "parent merged".to_string(),
            };
            report_harvest(&label, &what, &harvested);
            ds.next_parent += 1;
        } else if git_dir.join("CHERRY_PICK_HEAD").exists() {
            let merge_rr = rerere::merge_rr(&stalled_worktree)?;
            let picked = ds.delta.get(ds.next_pick).cloned().unwrap_or_default();
            if git::ok(&stalled_worktree, &["diff", "--cached", "--quiet", "HEAD"]) {
                // The resolution kept nothing: the commit's content is
                // already in the reconstruction, so there is nothing to
                // commit and the replay simply skips it.
                git::out(&stalled_worktree, &replay_cfg(&["cherry-pick", "--skip"]))?;
                println!(
                    "  {label} replayed {}: EMPTY after resolution -- skipped",
                    short(&picked)
                );
            } else {
                git::out(
                    &stalled_worktree,
                    &replay_cfg(&["cherry-pick", "--continue"]),
                )?;
                let harvested = rerere::harvest(&ctx.root, &stalled_worktree)?;
                rerere::index_add(&ctx.root, &entry.name, &harvested, &merge_rr)?;
                report_harvest(&label, &format!("replayed {}", short(&picked)), &harvested);
            }
            ds.next_pick += 1;
        }
        // Nothing in flight means the human committed the resolution by hand;
        // re-entering the reconstruction is then the whole of the repair.
        st.next_index = ds.entry_index;
        persist_derive(&ctx, &mut st, &ds)?;
    } else if let Some(mut pending) = st.pending.take() {
        // Stalled in a fixup: the merge is already committed, so only the
        // fixup's staged content needs a commit. Re-running the merge here
        // would duplicate it.
        let entry = &ctx.manifest.entries[stalled];
        let rel = entry.fixup.clone().with_context(|| {
            format!(
                "{}: the build stalled in a coherence fixup but the manifest no longer \
                 declares one; abandon the worktree and rebuild",
                entry.name
            )
        })?;
        if git::out(&ctx.worktree, &["diff", "--cached", "--name-only"])?.is_empty() {
            bail!(
                "{}: nothing staged for the fixup {rel}; resolve it and `git add`, \
                 or detach it with `fork-fold fixup {} --remove`",
                entry.name,
                entry.name
            );
        }
        let message = format!("fork-fold: fixup {}", entry.name);
        git::out(&ctx.worktree, &["commit", "-q", "-m", &message])?;
        pending.fixup = Some(patch_blob(&ctx.root, &rel)?);
        st.results.push(pending);
        st.next_index = stalled + 1;
        state::write(&ctx.worktree, &st)?;
        println!(
            "  {} fixup committed as resolved",
            label(stalled, &entry.name)
        );
        // The lock now pins a fixup blob whose patch does NOT reproduce what
        // was just committed, so a rebuild stalls here again. Say so plainly.
        println!(
            "  WARNING: {rel} still holds the version that failed; re-capture it with \
             `fork-fold fixup {} {rel} --capture` after this build, or the next \
             rebuild stops here again",
            entry.name
        );
    } else if git_dir.join("MERGE_HEAD").exists() {
        let entry = &ctx.manifest.entries[stalled];
        // Read MERGE_RR before committing: the rerere-enabled commit records
        // the postimages and clears it.
        let merge_rr = rerere::merge_rr(&ctx.worktree)?;
        git::out(&ctx.worktree, &rerere::with_cfg(&["commit", "--no-edit"]))?;
        let harvested = rerere::harvest(&ctx.root, &ctx.worktree)?;
        rerere::index_add(&ctx.root, &entry.name, &harvested, &merge_rr)?;
        // The tracked object, which is not always the merged one: a derived
        // entry merges its reconstruction, and the lock records its pin.
        let oid = match st.pins.get(&entry.name) {
            Some(pin) => pin.clone(),
            None => git::out(&ctx.worktree, &["rev-parse", "HEAD^2"])?,
        };
        let label = label(stalled, &entry.name);
        report_harvest(&label, "resolved", &harvested);
        let result = EntryResult {
            name: entry.name.clone(),
            oid,
            status: "merged".into(),
            conflicted: true,
            resolution: Some(rerere::label(&harvested)),
            fixup: None,
            derived: st.derived.get(&entry.name).cloned(),
        };
        // finish_entry persists the advance IMMEDIATELY: if the very next
        // entry errors, a stale index would re-merge this one and falsely
        // report it EMPTY. It also runs this entry's fixup, which can stall
        // in turn — the resolution and its fixup are one step.
        let done = finish_entry(&ctx, &mut st, stalled, entry, &label, result)?;
        clean_derive(&ctx, entry, done);
        if !done {
            return Ok(STOPPED);
        }
    } else if git_dir.join("MERGE_MSG").exists()
        || !git::out(&ctx.worktree, &["diff", "--cached", "--name-only"])?.is_empty()
    {
        // A stalled patch entry: commit the staged patch-entry result.
        let entry = &ctx.manifest.entries[stalled];
        if let Kind::Patch { .. } = &entry.kind {
            let message = format!("fork-fold: {}", entry.name);
            git::out(&ctx.worktree, &["commit", "-q", "-m", &message])?;
            let oid = st.pins.get(&entry.name).cloned().unwrap_or_default();
            st.results.push(EntryResult {
                name: entry.name.clone(),
                oid,
                status: "applied".into(),
                conflicted: true,
                resolution: None,
                fixup: None,
                derived: None,
            });
            st.next_index = stalled + 1;
            state::write(&ctx.worktree, &st)?;
        }
    }

    println!(
        "resuming at entry {}/{}",
        st.next_index + 1,
        ctx.manifest.entries.len()
    );
    let resume_at = st.next_index;
    match run_entries(&ctx, &mut st, resume_at)? {
        Some(code) => Ok(code),
        None => {
            let previous = lock::load(&ctx.root)?;
            finalize(&ctx, &st, previous.as_ref(), true)?;
            Ok(0)
        }
    }
}
