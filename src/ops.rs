//! Non-build verbs: update (the only pin-mover), status, prune, fixup.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Result};

use crate::engine::{self, derive, refuse, Ctx};
use crate::git::{self, short};
use crate::lock::{self, EntryResult, Prefix, Status};
use crate::manifest::{self, edit, entries_noun, Entry};
use crate::rerere;
use crate::source;
use crate::state;

/// Print how a pin moved, in the vocabulary every repin report shares.
fn report_pin(indent: &str, what: &str, old: Option<&str>, new: &str) {
    match old {
        Some(old) if old == new => println!("{indent}{what}: unchanged ({})", short(new)),
        Some(old) => println!("{indent}{what}: {} -> {}", short(old), short(new)),
        None => println!("{indent}{what}: pinned {}", short(new)),
    }
}

/// Repin the base and entries to live heads. `build` never moves existing
/// pins; this is the only verb that does.
pub fn update(root: &Path, names: &[String]) -> Result<()> {
    let ctx = Ctx::open(root, true)?;
    let mut lock = lock::load(root)?.unwrap_or_default();
    let all = names.is_empty();
    let wants = |name: &str| all || names.iter().any(|n| n == name);

    for name in names {
        if name != "base" && !ctx.manifest.has_entry(name) {
            bail!("no entry named {name:?} in the manifest");
        }
    }

    if wants("base") {
        let new = engine::fetch_base(&ctx)?;
        report_pin("  ", "base", lock.pins.base.as_deref(), &new);
        lock.pins.base = Some(new);
    }

    for entry in &ctx.manifest.entries {
        if !wants(&entry.name) {
            continue;
        }
        let new = engine::fetch_entry(&ctx, entry)?;
        let old = lock.pins.entries.insert(entry.name.clone(), new.clone());
        report_pin("  ", &entry.name, old.as_deref(), &new);
        if entry.is_derived() {
            update_derived(&ctx, &mut lock, entry, &new)?;
        }
    }

    lock::save(root, &lock)?;
    println!("wrote {}", lock::FILE);
    Ok(())
}

/// Repin a derived entry's parents and re-establish its anchor, in that order.
///
/// Both belong to `update` for the same reason the entry pin does: they say
/// what the next build reconstructs from, and a `build` that moved them could
/// produce a different tree from the same lock. The anchor comes last because
/// two of its three rules are questions about the pins this just moved, and
/// which rule fired is printed because the anchor decides which commits count
/// as the entry's own — an operator who cannot audit that boundary cannot tell
/// duplicated work from replayed work until it is in the tree.
fn update_derived(ctx: &Ctx, lock: &mut lock::Lock, entry: &Entry, pin: &str) -> Result<()> {
    let mut pins = lock
        .pins
        .parents
        .get(&entry.name)
        .cloned()
        .unwrap_or_default();
    for parent in &entry.parents {
        let new = engine::fetch_parent(ctx, entry, parent)?;
        let old = pins.insert(parent.name.clone(), new.clone());
        let what = format!("parent {}", parent.name);
        report_pin("    ", &what, old.as_deref(), &new);
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
    println!("    anchor {} -- {}", short(&anchor.oid), anchor.describe());
    lock.pins.parents.insert(entry.name.clone(), pins);
    lock.pins.anchors.insert(entry.name.clone(), anchor.oid);
    Ok(())
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

/// How a pinned topic stands against the pinned base, for `status`.
/// `absorbed` is the flag to print when the base already contains it.
fn base_flag(repo: &Path, pin: &str, base: &str, absorbed: &str) -> Option<String> {
    if !git::has_commit(repo, pin) {
        None
    } else if contained(repo, pin, base) {
        Some(absorbed.to_string())
    } else if refuse::conflicts_with_base(repo, base, pin) {
        // Reported here because `build` refuses this outright, and the
        // repair is in another repository. Better to learn it from a
        // status read than from a build that stops and rolls back.
        Some("CONFLICTS WITH BASE -- rebase the topic; build refuses it".to_string())
    } else {
        None
    }
}

fn flag_text(flags: &[String]) -> String {
    if flags.is_empty() {
        String::new()
    } else {
        format!("  [{}]", flags.join("; "))
    }
}

fn pin_text(pin: Option<&String>) -> String {
    pin.map_or("UNPINNED".to_string(), |p| short(p).to_string())
}

pub fn status(root: &Path, live: bool) -> Result<()> {
    let m = manifest::load(root)?;
    let lock = lock::load(root)?.unwrap_or_default();
    // Live checks fetch, so they need a configured source; offline ones
    // only need whatever repository already exists.
    let repo = if live {
        Some(source::source_repo(root, &m, true)?)
    } else {
        source::source_repo_if_present(root, &m)
    };
    let ctx = repo
        .as_ref()
        .filter(|_| live)
        .map(|repo| Ctx::new(root, m.clone(), repo.clone()));

    match &lock.pins.base {
        Some(base) => println!("base: {} ({}:{})", short(base), m.base.remote, m.base.ref_),
        None => println!("base: UNPINNED ({}:{})", m.base.remote, m.base.ref_),
    }
    if let Some(ctx) = &ctx {
        let head = engine::fetch_base(ctx)?;
        if Some(&head) != lock.pins.base.as_ref() {
            println!(
                "      live head {} -- run `fork-assembler update base`",
                short(&head)
            );
        }
    }

    let results: BTreeMap<&str, &EntryResult> = lock
        .build
        .as_ref()
        .map(|b| b.results.iter().map(|r| (r.name.as_str(), r)).collect())
        .unwrap_or_default();
    let recorded = rerere::index_entry_names(root)?;
    let base = lock.pins.base.as_deref();

    for entry in &m.entries {
        let pin = lock.pins.entries.get(&entry.name);
        let mut flags = Vec::new();
        if recorded.contains(&entry.name) {
            flags.push("rerere resolution".to_string());
        }
        if let Some(fixup) = &entry.fixup {
            if root.join(fixup).exists() {
                flags.push(format!("fixup {fixup}"));
            } else {
                flags.push(format!("fixup {fixup} MISSING -- build will fail"));
            }
        }
        if let Some(result) = results.get(entry.name.as_str()) {
            if !matches!(result.status, Status::Merged | Status::Applied) {
                flags.push(format!(
                    "last build: {}",
                    result.status.as_str().to_uppercase()
                ));
            }
        }
        if let (Some(pin), Some(base), Some(repo), false) =
            (pin, base, repo.as_ref(), entry.kind.is_patch())
        {
            flags.extend(base_flag(
                repo,
                pin,
                base,
                "contained in base -- prune candidate",
            ));
        }
        if let (Some(ctx), false) = (ctx.as_ref(), entry.kind.is_patch()) {
            let head = engine::fetch_entry(ctx, entry)?;
            if Some(&head) != pin {
                flags.push(format!("live head {} -- pin is behind", short(&head)));
            }
        }
        println!(
            "  {:<24} {} ({}){}",
            entry.name,
            pin_text(pin),
            entry.source(),
            flag_text(&flags)
        );

        // A derived entry's parents are not entries and never will be, so they
        // print beneath it: they are part of what this one step reconstructs,
        // and the anchor says where its own work starts.
        for parent in &entry.parents {
            let pin = lock
                .pins
                .parents
                .get(&entry.name)
                .and_then(|pins| pins.get(&parent.name));
            let mut flags = Vec::new();
            if let (Some(pin), Some(base), Some(repo)) = (pin, base, repo.as_ref()) {
                flags.extend(base_flag(
                    repo,
                    pin,
                    base,
                    "absorbed upstream -- consider removing it from parents",
                ));
            }
            if let Some(ctx) = ctx.as_ref() {
                let head = engine::fetch_parent(ctx, entry, parent)?;
                if Some(&head) != pin {
                    flags.push(format!("live head {} -- pin is behind", short(&head)));
                }
            }
            println!(
                "    {:<22} {} ({}){}",
                format!("parent {}", parent.name),
                pin_text(pin),
                parent.source(),
                flag_text(&flags)
            );
        }
        if entry.is_derived() {
            match lock.pins.anchors.get(&entry.name) {
                Some(anchor) => println!("    {:<22} {}", "anchor", short(anchor)),
                None => println!(
                    "    {:<22} UNRESOLVED -- the next build detects it",
                    "anchor"
                ),
            }
        }
    }

    // Exclusions are manifest intent with no pin and no step, so they print
    // after the stack rather than in it: what the build will never reach, and
    // what discovery must not re-admit.
    if !m.excludes.is_empty() {
        println!("\nexcluded:");
        for exclusion in &m.excludes {
            println!("  {}", exclusion.describe());
        }
    }

    match &lock.build {
        Some(build) => {
            println!("\nlast build: commit {}", short(&build.commit));
            println!("  tree {} ({} conflicts)", build.tree, build.conflicts);
            let fixups = engine::fixup_blobs(root, &m.entries, false)?;
            let snapshot = lock::snapshot(&m.entries, &lock.pins, &fixups);
            match lock::prefix_relation(&lock, &snapshot, base.unwrap_or_default()) {
                Prefix::Exact => println!("  manifest matches the lock: `build` is a no-op"),
                Prefix::Extension(at) => println!(
                    "  manifest extends the lock: `build` merges only entries {}..{} incrementally",
                    at + 1,
                    m.entries.len()
                ),
                Prefix::Diverged(reason) => {
                    println!("  full rebuild needed: {reason}")
                }
                Prefix::NoBuild => {}
            }
        }
        None => println!("\nno completed build recorded; run `fork-assembler build`"),
    }
    Ok(())
}

pub fn prune(root: &Path, dry_run: bool) -> Result<()> {
    let m = manifest::load(root)?;
    let lock = lock::load(root)?.unwrap_or_default();
    let Some(base) = lock.pins.base.clone() else {
        bail!("no base pin; run `fork-assembler build` or `update` first");
    };
    let Some(repo) = source::source_repo_if_present(root, &m) else {
        bail!("no source repository available; run `fork-assembler build` first");
    };

    let mut dead = Vec::new();
    for entry in &m.entries {
        if entry.kind.is_patch() {
            continue;
        }
        let Some(pin) = lock.pins.entries.get(&entry.name) else {
            continue;
        };
        if git::has_commit(&repo, pin) && contained(&repo, pin, &base) {
            println!("  {}: contained in base ({})", entry.name, short(pin));
            dead.push(entry.name.clone());
        }
    }
    if dead.is_empty() {
        println!("nothing to prune: no pinned entry is contained in the base");
        return Ok(());
    }
    let count = format!("{} {}", dead.len(), entries_noun(dead.len()));
    if dry_run {
        println!("would remove {count} (dry run)");
        return Ok(());
    }
    let removal = edit::remove_entries(root, &dead)?;
    println!(
        "removed {count}; the build suffix from position {} is invalidated -- run `fork-assembler build`",
        removal.earliest + 1
    );
    report_removal(&removal);
    Ok(())
}

/// What a removal detached, so nothing silently vanishes from the build.
///
/// A coherence fixup repairs an interaction BETWEEN entries, so removing one
/// side does not mean the incoherence is gone — an entry that landed upstream
/// usually still clashes with the topic it clashed with before. The patch is
/// left on disk and surfaced here.
///
/// A parent was kept out of the entry list by the declaration that just left
/// with its entry. Unlike a fixup it leaves no file to re-home — it leaves a
/// silence, and a silence is exactly what discovery overwrites: an open PR
/// that was somebody's parent yesterday is an ordinary candidate today.
pub fn report_removal(removal: &edit::Removal) {
    for (name, path) in &removal.orphaned_fixups {
        println!(
            "  NOTE: {name} carried the coherence fixup {path}, now unreferenced.\n  \
             {path} is left on disk: if the incoherence it repaired survives the removal, \
             re-home it with `fork-assembler fixup OTHER_ENTRY {path}`; otherwise delete it."
        );
    }
    for (name, parents) in &removal.orphaned_parents {
        println!(
            "  NOTE: {name} declared {} as parent(s); nothing references them now.\n  \
             Their commits left the stack with {name}. If they are still open PRs, the next \
             `add --prs-from` sweep will offer them again -- carry them as entries if they \
             should be in the stack on their own, or `fork-assembler exclude` them with a reason \
             if they should not.",
            parents.join(", ")
        );
    }
}

pub fn remove(root: &Path, name: String) -> Result<()> {
    let removal = edit::remove_entries(root, &[name])?;
    println!(
        "entry removed; the build suffix from position {} is invalidated -- run `fork-assembler build`",
        removal.earliest + 1
    );
    report_removal(&removal);
    Ok(())
}

/// Attach, re-capture, or detach an entry's coherence fixup.
pub fn fixup(
    root: &Path,
    name: &str,
    path: Option<&str>,
    capture: bool,
    remove: bool,
) -> Result<()> {
    if remove {
        let index = edit::set_fixup(root, name, None)?;
        println!("detached {name}'s coherence fixup (the patch file is left in place)");
        println!("entry {} changed -- run `fork-assembler build`", index + 1);
        return Ok(());
    }
    let Some(rel) = path else {
        bail!("pass the fixup's patch path, or --remove to detach it");
    };

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
        let (diff, source) = capture_diff(&worktree, name)?;
        if diff.is_empty() {
            bail!(
                "nothing to capture: the build worktree has no uncommitted changes and no \
                 `fork-assembler: fixup {name}` commit"
            );
        }
        std::fs::write(&dest, diff)?;
        println!("captured {rel} from {source}");
        // A capture during a stalled build supersedes that build: the fixup
        // blob just changed, so the stalled worktree can only be rebuilt.
        let _ = state::clear(&worktree);
    } else if !root.join(rel).exists() {
        bail!(
            "patch file {rel} does not exist (pass --capture to write it from the build worktree)"
        );
    }

    let index = edit::set_fixup(root, name, Some(rel))?;
    println!("{name}: coherence fixup set to {rel}");
    println!(
        "entry {} now produces a different tree -- run `fork-assembler build`",
        index + 1
    );
    Ok(())
}

/// The diff a capture should record: the build worktree's uncommitted changes
/// when there are any (the state at a fixup stall — precisely the corrected
/// fixup), else the entry's already-committed fixup commit (the state after
/// `continue` committed a manual resolution).
fn capture_diff(worktree: &Path, name: &str) -> Result<(Vec<u8>, String)> {
    // Intent-to-add so newly created files appear in the diff.
    let _ = git::raw(worktree, &["add", "-A", "-N"]);
    let pending = git::bytes(worktree, &["diff", "--binary", "HEAD"])?;
    if !pending.is_empty() {
        return Ok((pending, "the build worktree's uncommitted changes".into()));
    }
    let subject = format!("fork-assembler: fixup {name}");
    let log = git::out(worktree, &["log", "--format=%H%x1f%s"])?;
    let commit = log
        .lines()
        .find_map(|line| line.split_once('\x1f').filter(|(_, s)| *s == subject))
        .map(|(hash, _)| hash.to_string());
    let Some(commit) = commit else {
        return Ok((Vec::new(), String::new()));
    };
    let diff = git::bytes(worktree, &["show", "--binary", "--format=", &commit])?;
    Ok((diff, format!("commit {} ({subject})", short(&commit))))
}
