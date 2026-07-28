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
        lock.pins.entries.insert(entry.name.clone(), new);
    }

    lock::save(root, &lock)?;
    println!("wrote {}", lock::FILE);
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
                println!("      live head {} -- run `fork-fold update base`", short(&head));
            }
        }
    }

    let results: std::collections::BTreeMap<&str, &lock::EntryResult> = lock
        .build
        .as_ref()
        .map(|b| b.results.iter().map(|r| (r.name.as_str(), r)).collect())
        .unwrap_or_default();

    for entry in &m.entries {
        let pin = lock.pins.entries.get(&entry.name);
        let mut flags = Vec::new();
        if crate::resolution::load(root, &entry.name)?.is_some() {
            flags.push("resolution".to_string());
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
        let pin_text = pin.map(|p| short(p).to_string()).unwrap_or("UNPINNED".into());
        let flag_text = if flags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", flags.join("; "))
        };
        println!("  {:<24} {} ({}){}", entry.name, pin_text, entry.source(), flag_text);
    }

    match &lock.build {
        Some(build) => {
            println!("\nlast build: commit {}", short(&build.commit));
            println!("  tree {} ({} conflicts)", build.tree, build.conflicts);
            let snapshot = lock::snapshot(&m.entries, &lock.pins.entries);
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
    let earliest = manifest::remove_entries(root, &dead)?;
    println!(
        "removed {} entr{}; the build suffix from position {} is invalidated -- run `fork-fold build`",
        dead.len(),
        if dead.len() == 1 { "y" } else { "ies" },
        earliest + 1
    );
    Ok(())
}
