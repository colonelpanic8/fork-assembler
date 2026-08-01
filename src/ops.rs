//! Non-build verbs: update (the only pin-mover), status, prune.

use std::path::Path;

use anyhow::{bail, Result};

use crate::engine::{self, Ctx};
use crate::git;
use crate::lock::{self, Prefix};
use crate::manifest::{self, Kind};
use crate::source;

fn short(oid: &str) -> &str {
    &oid[..12.min(oid.len())]
}

/// Repin the base and entries to live heads. `build` never moves existing
/// pins; this is the only verb that does.
pub fn update(root: &Path, names: &[String]) -> Result<()> {
    let ctx = Ctx::open(root, true)?;
    let mut lock = lock::load(root)?.unwrap_or_default();
    let all = names.is_empty();
    let wants = |name: &str| all || names.iter().any(|n| n == name);

    for name in names {
        if name != "base" && !ctx.manifest.entries.iter().any(|e| &e.name == name) {
            bail!("no entry named {name:?} in the manifest");
        }
    }

    if wants("base") {
        let old = lock.pins.base.clone();
        let new = engine::fetch_base(&ctx)?;
        match old {
            Some(old) if old == new => println!("  base: unchanged ({})", short(&new)),
            Some(old) => println!("  base: {} -> {}", short(&old), short(&new)),
            None => println!("  base: pinned {}", short(&new)),
        }
        lock.pins.base = Some(new);
    }

    for entry in &ctx.manifest.entries {
        if !wants(&entry.name) {
            continue;
        }
        let new = engine::fetch_entry(&ctx, entry)?;
        let old = lock.pins.entries.get(&entry.name).cloned();
        match old {
            Some(old) if old == new => println!("  {}: unchanged ({})", entry.name, short(&new)),
            Some(old) => println!("  {}: {} -> {}", entry.name, short(&old), short(&new)),
            None => println!("  {}: pinned {}", entry.name, short(&new)),
        }
        lock.pins.entries.insert(entry.name.clone(), new.clone());
        if !entry.parents.is_empty() {
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
fn update_derived(
    ctx: &Ctx,
    lock: &mut lock::Lock,
    entry: &manifest::Entry,
    pin: &str,
) -> Result<()> {
    let mut pins = lock
        .pins
        .parents
        .get(&entry.name)
        .cloned()
        .unwrap_or_default();
    for parent in &entry.parents {
        let new = engine::fetch_parent(ctx, entry, parent)?;
        match pins.get(&parent.name) {
            Some(old) if *old == new => {
                println!("    parent {}: unchanged ({})", parent.name, short(&new))
            }
            Some(old) => println!(
                "    parent {}: {} -> {}",
                parent.name,
                short(old),
                short(&new)
            ),
            None => println!("    parent {}: pinned {}", parent.name, short(&new)),
        }
        pins.insert(parent.name.clone(), new);
    }
    pins.retain(|name, _| entry.parents.iter().any(|p| &p.name == name));

    let previous_base_tip = lock
        .build
        .as_ref()
        .and_then(|b| b.results.iter().find(|r| r.name == entry.name))
        .and_then(|r| r.derived.as_ref())
        .map(|d| d.base_tip.clone());
    let anchor = engine::resolve_anchor(
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
    if git::ok(repo, &["merge-base", "--is-ancestor", oid, base]) {
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

pub fn status(root: &Path, live: bool) -> Result<()> {
    let m = manifest::load(root)?;
    let lck = lock::load(root)?;
    let lock = lck.clone().unwrap_or_default();
    let repo = if live {
        Some(source::source_repo(root, &m, true)?)
    } else {
        source::source_repo_if_present(root, &m)
    };
    let ctx = repo.as_ref().map(|r| Ctx {
        root: root.to_path_buf(),
        manifest: manifest::load(root).expect("manifest already loaded once"),
        repo: r.clone(),
        worktree: root.join(engine::WORKTREE),
    });

    match &lock.pins.base {
        Some(base) => println!("base: {} ({}:{})", short(base), m.base.remote, m.base.ref_),
        None => println!("base: UNPINNED ({}:{})", m.base.remote, m.base.ref_),
    }
    if live {
        if let Some(ctx) = &ctx {
            let head = engine::fetch_base(ctx)?;
            if Some(&head) != lock.pins.base.as_ref() {
                println!(
                    "      live head {} -- run `fork-fold update base`",
                    short(&head)
                );
            }
        }
    }

    let results: std::collections::BTreeMap<&str, &lock::EntryResult> = lock
        .build
        .as_ref()
        .map(|b| b.results.iter().map(|r| (r.name.as_str(), r)).collect())
        .unwrap_or_default();
    let recorded = crate::rerere::index_entry_names(root)?;

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
            if result.status != "merged" && result.status != "applied" {
                flags.push(format!("last build: {}", result.status.to_uppercase()));
            }
        }
        if let (Some(pin), Some(base), Some(repo), false) = (
            pin,
            lock.pins.base.as_ref(),
            repo.as_ref(),
            matches!(entry.kind, Kind::Patch { .. }),
        ) {
            if git::has_commit(repo, pin) && contained(repo, pin, base) {
                flags.push("contained in base -- prune candidate".to_string());
            }
        }
        if live {
            if let (Some(ctx), false) = (ctx.as_ref(), matches!(entry.kind, Kind::Patch { .. })) {
                let head = engine::fetch_entry(ctx, entry)?;
                if Some(&head) != pin {
                    flags.push(format!("live head {} -- pin is behind", short(&head)));
                }
            }
        }
        let pin_text = pin
            .map(|p| short(p).to_string())
            .unwrap_or("UNPINNED".into());
        let flag_text = if flags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", flags.join("; "))
        };
        println!(
            "  {:<24} {} ({}){}",
            entry.name,
            pin_text,
            entry.source(),
            flag_text
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
            if let (Some(pin), Some(base), Some(repo)) =
                (pin, lock.pins.base.as_ref(), repo.as_ref())
            {
                if git::has_commit(repo, pin) && contained(repo, pin, base) {
                    flags
                        .push("absorbed upstream -- consider removing it from parents".to_string());
                }
            }
            if let Some(ctx) = ctx.as_ref().filter(|_| live) {
                let head = engine::fetch_parent(ctx, entry, parent)?;
                if Some(&head) != pin {
                    flags.push(format!("live head {} -- pin is behind", short(&head)));
                }
            }
            println!(
                "    {:<22} {} ({}){}",
                format!("parent {}", parent.name),
                pin.map(|p| short(p).to_string())
                    .unwrap_or("UNPINNED".into()),
                parent.source(),
                if flags.is_empty() {
                    String::new()
                } else {
                    format!("  [{}]", flags.join("; "))
                }
            );
        }
        if !entry.parents.is_empty() {
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
            let base_pin = lock.pins.base.clone().unwrap_or_default();
            match lock::prefix_relation(&lock, &snapshot, &base_pin) {
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
        None => println!("\nno completed build recorded; run `fork-fold build`"),
    }
    Ok(())
}

pub fn prune(root: &Path, dry_run: bool) -> Result<()> {
    let m = manifest::load(root)?;
    let lock = lock::load(root)?.unwrap_or_default();
    let Some(base) = lock.pins.base.clone() else {
        bail!("no base pin; run `fork-fold build` or `update` first");
    };
    let Some(repo) = source::source_repo_if_present(root, &m) else {
        bail!("no source repository available; run `fork-fold build` first");
    };

    let mut dead = Vec::new();
    for entry in &m.entries {
        if matches!(entry.kind, Kind::Patch { .. }) {
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
    if dry_run {
        println!(
            "would remove {} entr{} (dry run)",
            dead.len(),
            if dead.len() == 1 { "y" } else { "ies" }
        );
        return Ok(());
    }
    let removal = manifest::remove_entries(root, &dead)?;
    println!(
        "removed {} entr{}; the build suffix from position {} is invalidated -- run `fork-fold build`",
        dead.len(),
        if dead.len() == 1 { "y" } else { "ies" },
        removal.earliest + 1
    );
    report_orphaned_fixups(&removal);
    report_orphaned_parents(&removal);
    Ok(())
}

/// A coherence fixup repairs an interaction BETWEEN entries, so removing one
/// side does not mean the incoherence is gone — an entry that landed upstream
/// usually still clashes with the topic it clashed with before. Surface the
/// detached patch instead of letting it silently vanish from the build.
pub fn report_orphaned_fixups(removal: &manifest::Removal) {
    for (name, path) in &removal.orphaned_fixups {
        println!(
            "  NOTE: {name} carried the coherence fixup {path}, now unreferenced.\n  \
             {path} is left on disk: if the incoherence it repaired survives the removal, \
             re-home it with `fork-fold fixup OTHER_ENTRY {path}`; otherwise delete it."
        );
    }
}

/// A parent was kept out of the entry list by the declaration that just left
/// with its entry. Unlike a fixup it leaves no file to re-home — it leaves a
/// silence, and a silence is exactly what discovery overwrites: an open PR
/// that was somebody's parent yesterday is an ordinary candidate today.
pub fn report_orphaned_parents(removal: &manifest::Removal) {
    for (name, parents) in &removal.orphaned_parents {
        println!(
            "  NOTE: {name} declared {} as parent(s); nothing references them now.\n  \
             Their commits left the stack with {name}. If they are still open PRs, the next \
             `add --prs-from` sweep will offer them again -- carry them as entries if they \
             should be in the stack on their own, or `fork-fold exclude` them with a reason \
             if they should not.",
            parents.join(", ")
        );
    }
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
        let index = manifest::set_fixup(root, name, None)?;
        println!("detached {name}'s coherence fixup (the patch file is left in place)");
        println!("entry {} changed -- run `fork-fold build`", index + 1);
        return Ok(());
    }
    let Some(rel) = path else {
        bail!("pass the fixup's patch path, or --remove to detach it");
    };

    if capture {
        let worktree = root.join(engine::WORKTREE);
        if !worktree.exists() {
            bail!(
                "no build worktree at {} to capture from; run `fork-fold build` first",
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
                 `fork-fold: fixup {name}` commit"
            );
        }
        std::fs::write(&dest, diff)?;
        println!("captured {rel} from {source}");
        // A capture during a stalled build supersedes that build: the fixup
        // blob just changed, so the stalled worktree can only be rebuilt.
        let _ = crate::state::clear(&worktree);
    } else if !root.join(rel).exists() {
        bail!(
            "patch file {rel} does not exist (pass --capture to write it from the build worktree)"
        );
    }

    let index = manifest::set_fixup(root, name, Some(rel))?;
    println!("{name}: coherence fixup set to {rel}");
    println!(
        "entry {} now produces a different tree -- run `fork-fold build`",
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
    let subject = format!("fork-fold: fixup {name}");
    let log = git::out(worktree, &["log", "--format=%H%x1f%s"])?;
    let commit = log
        .lines()
        .find_map(|line| line.split_once('\x1f').filter(|(_, s)| *s == subject))
        .map(|(hash, _)| hash.to_string());
    let Some(commit) = commit else {
        return Ok((Vec::new(), String::new()));
    };
    let diff = git::bytes(worktree, &["show", "--binary", "--format=", &commit])?;
    Ok((
        diff,
        format!("commit {} ({subject})", &commit[..12.min(commit.len())]),
    ))
}
