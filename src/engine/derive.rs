//! Reconstructing derived entries.
//!
//! A derived entry's pin is stale by construction: it was built against
//! whatever its parents were at the time. `build` re-merges the parents onto
//! the pinned base, replays the entry's own commits on top, and merges that
//! reconstruction into the stack in place of the pin. The anchor decides which
//! commits count as the entry's own.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{bail, Context, Result};

use super::pins::derived_ref;
use super::refuse::{refuse_if_base_conflict, Topic};
use super::{conflicted_files, prepare_worktree, remove_worktree, report_harvest, Ctx};
use crate::git;
use crate::lock::{self, DerivedResult};
use crate::manifest::Entry;
use crate::rerere;
use crate::state::{self, DeriveState, State};

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
    let still_below_pin =
        |oid: &str| git::has_commit(repo, oid) && git::is_ancestor(repo, oid, pin);
    if let Some(base_tip) = previous_base_tip.filter(|oid| still_below_pin(oid)) {
        return Ok(Anchor {
            oid: base_tip.to_string(),
            rule: AnchorRule::PushedReconstruction,
        });
    }
    if let Some(anchor) = previous_anchor.filter(|oid| still_below_pin(oid)) {
        return Ok(Anchor {
            oid: anchor.to_string(),
            rule: AnchorRule::Kept,
        });
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
        git::short(pin),
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

/// git config for the replay phase: the rerere pair machinery plus an editor
/// that exits successfully without opening anything. `cherry-pick` and its
/// `--continue` want an editor by default, and a build has no terminal to give
/// them one.
fn replay_cfg<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut cfg = vec!["-c", "core.editor=true"];
    cfg.extend(rerere::with_cfg(args));
    cfg
}

/// Persist the reconstruction's progress. Every step, like the entry loop:
/// resuming a merge sequence or a cherry-pick sequence at a stale index
/// re-applies work that is already in.
fn persist(ctx: &Ctx, st: &mut State, ds: &DeriveState) -> Result<()> {
    st.derive = Some(ds.clone());
    state::write(&ctx.worktree, st)
}

fn stop(ctx: &Ctx, entry: &Entry, label: &str, doing: &str, unresolved: &[String]) {
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
    println!("  Stage with `git add`, then: fork-assembler continue");
}

/// True when the derive worktree's index holds nothing beyond HEAD: a replay
/// whose content is already present.
fn nothing_staged(derive: &Path) -> bool {
    git::ok(derive, &["diff", "--cached", "--quiet", "HEAD"])
}

/// Reconstruct a derived entry: re-merge its parents onto the pinned base,
/// then replay the entry's own commits on top.
///
/// Returns None when the reconstruction stopped for the human, having already
/// persisted where it got to.
pub fn reconstruct(
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
                git::short(&st.base),
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
    persist(ctx, st, &ds)?;

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
        if git::is_ancestor(&ctx.repo, &parent_pin, &head) {
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
                "fork-assembler: merge parent {} (for {})",
                parent.name, entry.name
            );
            let out = git::raw(
                &derive,
                &rerere::with_cfg(&["merge", "--no-ff", "--no-edit", "-m", &message, &parent_pin]),
            )?;
            if out.status.success() {
                println!(
                    "  {label} parent {} merged {}",
                    parent.name,
                    git::short(&parent_pin)
                );
            } else {
                // Before anything else: a parent that cannot merge with the
                // base on its own is out of date with upstream, and no amount
                // of reconstruction here makes it apply again.
                refuse_if_base_conflict(
                    ctx,
                    entry,
                    Topic::parent(entry, parent, &parent_pin),
                    &st.base,
                )?;
                st.conflicts += 1;
                let unresolved = conflicted_files(&derive)?;
                if !unresolved.is_empty() {
                    persist(ctx, st, &ds)?;
                    let doing = format!("merging parent {}", parent.name);
                    stop(ctx, entry, label, &doing, &unresolved);
                    return Ok(None);
                }
                git::out(&derive, &rerere::with_cfg(&["commit", "--no-edit"]))?;
                println!(
                    "  {label} parent {} merged; auto-resolved from tracked rerere pairs",
                    parent.name
                );
            }
        }
        ds.next_parent += 1;
        persist(ctx, st, &ds)?;
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
        persist(ctx, st, &ds)?;
    }

    while ds.next_pick < ds.delta.len() {
        let commit = ds.delta[ds.next_pick].clone();
        let subject = git::out(&ctx.repo, &["log", "-1", "--format=%s", &commit])?;
        let out = git::raw(&derive, &replay_cfg(&["cherry-pick", &commit]))?;
        if out.status.success() {
            println!("  {label} replayed {} ({subject})", git::short(&commit));
        } else {
            let unresolved = conflicted_files(&derive)?;
            if !unresolved.is_empty() {
                st.conflicts += 1;
                persist(ctx, st, &ds)?;
                let doing = format!("replaying {} ({subject})", git::short(&commit));
                stop(ctx, entry, label, &doing, &unresolved);
                return Ok(None);
            }
            // Nothing conflicted, so either rerere replayed every hunk and
            // staged the result, or the commit's content is already present
            // and there is nothing left to commit at all.
            if nothing_staged(&derive) {
                git::out(&derive, &replay_cfg(&["cherry-pick", "--skip"]))?;
                println!(
                    "  {label} replayed {} ({subject}): EMPTY -- already present",
                    git::short(&commit)
                );
            } else {
                st.conflicts += 1;
                git::out(&derive, &replay_cfg(&["cherry-pick", "--continue"]))?;
                println!(
                    "  {label} replayed {} ({subject}); auto-resolved from tracked rerere pairs",
                    git::short(&commit)
                );
            }
        }
        ds.next_pick += 1;
        persist(ctx, st, &ds)?;
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
        git::short(&anchor.oid),
        anchor.describe()
    );
    st.anchors.insert(entry.name.clone(), anchor.oid.clone());
    state::write(&ctx.worktree, st)?;
    Ok(anchor.oid)
}

/// Drop the reconstruction worktree once its entry's step has completed.
///
/// It survives an unfinished step on purpose: when a build stops on the stack
/// merge or on a fixup, the reconstruction that produced the conflicting side
/// is exactly what the operator needs to read next to it.
pub fn clean(ctx: &Ctx, entry: &Entry, step_completed: bool) {
    if step_completed && entry.is_derived() {
        remove_worktree(ctx, &ctx.derive_worktree());
    }
}

/// Finish whatever `continue` found open in the derive worktree — a parent
/// merge or a cherry-pick the human resolved — and advance that sequence's
/// index, so the entry loop re-enters the reconstruction where it left off.
/// Nothing in flight means the human committed the resolution by hand, and
/// re-entering is then the whole of the repair.
pub fn resume(ctx: &Ctx, st: &mut State, mut ds: DeriveState, label: &str) -> Result<()> {
    let derive = ctx.derive_worktree();
    let git_dir = git::git_dir(&derive)?;
    let entry = &ctx.manifest.entries[ds.entry_index];
    if git_dir.join("MERGE_HEAD").exists() {
        // Read MERGE_RR before committing: the rerere-enabled commit
        // records the postimages and clears it.
        let merge_rr = rerere::merge_rr(&derive)?;
        git::out(&derive, &rerere::with_cfg(&["commit", "--no-edit"]))?;
        let harvested = rerere::harvest(&ctx.root, &derive)?;
        // Attributed to the entry, not the parent: the entry is what the
        // manifest carries and what a later build will replay this for.
        rerere::index_add(&ctx.root, &entry.name, &harvested, &merge_rr)?;
        let what = match entry.parents.get(ds.next_parent) {
            Some(parent) => format!("parent {} merged", parent.name),
            None => "parent merged".to_string(),
        };
        report_harvest(label, &what, &harvested);
        ds.next_parent += 1;
    } else if git_dir.join("CHERRY_PICK_HEAD").exists() {
        let merge_rr = rerere::merge_rr(&derive)?;
        let picked = ds.delta.get(ds.next_pick).cloned().unwrap_or_default();
        if nothing_staged(&derive) {
            // The resolution kept nothing: the commit's content is already in
            // the reconstruction, so there is nothing to commit and the
            // replay simply skips it.
            git::out(&derive, &replay_cfg(&["cherry-pick", "--skip"]))?;
            println!(
                "  {label} replayed {}: EMPTY after resolution -- skipped",
                git::short(&picked)
            );
        } else {
            git::out(&derive, &replay_cfg(&["cherry-pick", "--continue"]))?;
            let harvested = rerere::harvest(&ctx.root, &derive)?;
            rerere::index_add(&ctx.root, &entry.name, &harvested, &merge_rr)?;
            let what = format!("replayed {}", git::short(&picked));
            report_harvest(label, &what, &harvested);
        }
        ds.next_pick += 1;
    }
    st.next_index = ds.entry_index;
    persist(ctx, st, &ds)
}

/// Publish completed derived-entry reconstructions only after every entry has
/// assembled successfully. This keeps a later stack conflict from updating a
/// review branch with a reconstruction that never became an assembled build.
///
/// A locked build is a read-only reproducibility check: it deliberately skips
/// this network write even when the manifest requests publication.
pub fn publish(ctx: &Ctx, st: &State, locked: bool) -> Result<()> {
    for entry in &ctx.manifest.entries {
        let Some(target) = &entry.reconstruction_publish else {
            continue;
        };
        let Some(tip) = st
            .results
            .iter()
            .find(|result| result.name == entry.name)
            .and_then(|result| result.derived.as_ref())
            .map(|derived| derived.tip.as_str())
        else {
            // An absorbed or empty entry has no newly reconstructed branch to
            // publish. Its normal build result already explains why.
            continue;
        };
        if locked {
            println!(
                "  {} reconstruction publication to {} skipped (--locked)",
                entry.name,
                target.source()
            );
            continue;
        }

        let destination = format!("refs/heads/{}", target.branch);
        let out = git::raw(
            &ctx.repo,
            &["ls-remote", "--heads", &target.remote, &destination],
        )?;
        if !out.status.success() {
            bail!(
                "{}: could not read reconstruction publish target {}: {}",
                entry.name,
                target.source(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let expected = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .map(str::to_string)
            .unwrap_or_default();
        let lease = format!("--force-with-lease={destination}:{expected}");
        let source = format!("{tip}:{destination}");
        git::out(&ctx.repo, &["push", &lease, &target.remote, &source]).with_context(|| {
            format!(
                "{}: publishing reconstructed {} to {}",
                entry.name,
                git::short(tip),
                target.source()
            )
        })?;
        println!(
            "  {} published reconstruction {} -> {}",
            entry.name,
            git::short(tip),
            target.source()
        );
    }
    Ok(())
}
