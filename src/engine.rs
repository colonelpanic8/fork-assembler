//! The build engine: assemble the stack from the lock's pins.
//!
//! `build` never moves an existing pin (`update` is the only verb that does);
//! entries with no pin yet get pinned from live refs on their first build.
//! The assembled branch is compiled output. `tree` (pre-provenance) is the
//! reproducibility invariant.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;

use crate::git;
use crate::lock::{self, EntryResult, Lock, Prefix};
use crate::manifest::{self, Entry, Kind, Manifest};
use crate::rerere;
use crate::source;
use crate::state::{self, State};

pub const WORKTREE: &str = ".worktrees/build";

/// Exit code signalling "stopped for human resolution".
pub const STOPPED: i32 = 2;

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
}

/// A private ref namespace so fetched heads never collide with user refs.
fn holding_ref(entry: &Entry) -> String {
    format!("refs/fork-fold/{}", manifest::sanitize_name(&entry.name))
}

pub fn fetch_entry(ctx: &Ctx, entry: &Entry) -> Result<String> {
    match &entry.kind {
        Kind::Branch { remote, branch, .. } => {
            let spec = format!("+refs/heads/{branch}:{}", holding_ref(entry));
            git::out(&ctx.repo, &["fetch", remote, &spec])?;
            git::out(&ctx.repo, &["rev-parse", &holding_ref(entry)])
        }
        Kind::Pr { remote, number } => {
            let spec = format!("+refs/pull/{number}/head:{}", holding_ref(entry));
            git::out(&ctx.repo, &["fetch", remote, &spec])?;
            git::out(&ctx.repo, &["rev-parse", &holding_ref(entry)])
        }
        Kind::Patch { path } => patch_blob(&ctx.root, path),
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
    println!("  pinned {} -> {}", entry.name, &oid[..12.min(oid.len())]);
    pins.insert(entry.name.clone(), oid.clone());
    Ok(oid)
}

fn prepare_worktree(ctx: &Ctx, at: &str) -> Result<()> {
    if ctx.worktree.exists() {
        let _ = git::raw(
            &ctx.repo,
            &[
                "worktree",
                "remove",
                "--force",
                &ctx.worktree.to_string_lossy(),
            ],
        );
    }
    // A deleted-but-registered worktree (e.g. the directory was rm -rf'd)
    // blocks re-adding at the same path.
    let _ = git::raw(&ctx.repo, &["worktree", "prune"]);
    if let Some(parent) = ctx.worktree.parent() {
        std::fs::create_dir_all(parent)?;
    }
    git::out(
        &ctx.repo,
        &[
            "worktree",
            "add",
            "--detach",
            &ctx.worktree.to_string_lossy(),
            at,
        ],
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

/// Apply one patch entry. Ok(true) = applied or already applied; Ok(false) =
/// failed, conflict left for the human.
fn apply_patch_entry(ctx: &Ctx, entry: &Entry, rel: &str) -> Result<Option<&'static str>> {
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
    let message = format!("fork-fold: {}", entry.name);
    git::out(&ctx.worktree, &["commit", "-q", "-m", &message])?;
    Ok(Some("applied"))
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
            match apply_patch_entry(ctx, entry, &rel)? {
                Some(outcome) => {
                    println!("  {label} {outcome}");
                    st.results.push(EntryResult {
                        name: entry.name.clone(),
                        oid,
                        status: "applied".into(),
                        conflicted: false,
                        resolution: None,
                    });
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
            st.next_index = index + 1;
            state::write(&ctx.worktree, st)?;
            continue;
        }

        if git::ok(&ctx.repo, &["merge-base", "--is-ancestor", &oid, &st.base]) {
            println!("  {label} ABSORBED upstream -- drop candidate");
            st.results.push(EntryResult {
                name: entry.name.clone(),
                oid,
                status: "absorbed".into(),
                conflicted: false,
                resolution: None,
            });
            st.next_index = index + 1;
            state::write(&ctx.worktree, st)?;
            continue;
        }

        let before = git::out(&ctx.worktree, &["rev-parse", "HEAD^{tree}"])?;
        let clean = merge_entry(ctx, entry, &oid)?;

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
                st.results.push(EntryResult {
                    name: entry.name.clone(),
                    oid,
                    status: "merged".into(),
                    conflicted: true,
                    resolution: Some(rerere::label(&hashes)),
                });
                st.next_index = index + 1;
                state::write(&ctx.worktree, st)?;
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
            println!("  {label} merged {}", &oid[..12.min(oid.len())]);
            "merged"
        };
        st.results.push(EntryResult {
            name: entry.name.clone(),
            oid,
            status: status.into(),
            conflicted: false,
            resolution: None,
        });
        st.next_index = index + 1;
        state::write(&ctx.worktree, st)?;
    }
    Ok(None)
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
            manifest_entries: lock::snapshot(&ctx.manifest.entries, &st.pins),
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
        .map(|l| l.pins.entries.clone())
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
        ensure_pin(&ctx, entry, &mut pins, locked)?;
    }

    // Decide full rebuild vs incremental extension vs up-to-date.
    let snapshot = lock::snapshot(&ctx.manifest.entries, &pins);
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
        println!("building from base {}", &base[..12.min(base.len())]);
        results.clear();
    }

    prepare_worktree(&ctx, &start_commit)?;
    let seeded = rerere::seed(&ctx.root, &ctx.worktree)?;
    if seeded > 0 {
        println!("seeded {seeded} tracked rerere pair(s)");
    }
    let mut st = State {
        next_index: start,
        base: base.clone(),
        pins,
        results,
        conflicts: 0,
        extended_from,
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

pub fn cont(root: &Path) -> Result<i32> {
    let ctx = Ctx::open(root, false).or_else(|_| Ctx::open(root, true))?;
    let Some(mut st) = state::read(&ctx.worktree)? else {
        bail!("no in-progress build found; run `fork-fold build`");
    };
    let unresolved = conflicted_files(&ctx.worktree)?;
    if !unresolved.is_empty() {
        bail!(
            "the build worktree still has unresolved conflicts:\n  {}",
            unresolved.join("\n  ")
        );
    }

    let git_dir = git::out(&ctx.worktree, &["rev-parse", "--git-dir"])?;
    let mut git_dir = PathBuf::from(git_dir);
    if !git_dir.is_absolute() {
        git_dir = ctx.worktree.join(git_dir);
    }

    let stalled = st.next_index;
    if git_dir.join("MERGE_HEAD").exists() {
        let entry = &ctx.manifest.entries[stalled];
        // Read MERGE_RR before committing: the rerere-enabled commit records
        // the postimages and clears it.
        let merge_rr = rerere::merge_rr(&ctx.worktree)?;
        git::out(&ctx.worktree, &rerere::with_cfg(&["commit", "--no-edit"]))?;
        let harvested = rerere::harvest(&ctx.root, &ctx.worktree)?;
        rerere::index_add(&ctx.root, &entry.name, &harvested, &merge_rr)?;
        let oid = git::out(&ctx.worktree, &["rev-parse", "HEAD^2"])?;
        if harvested.is_empty() {
            println!(
                "  [{:2}/{}] {:<24} resolved; WARNING: no rerere pair captured \
                 (unrecognizable conflict) -- a rebuild will stop here again",
                stalled + 1,
                ctx.manifest.entries.len(),
                entry.name,
            );
        } else {
            println!(
                "  [{:2}/{}] {:<24} resolved; harvested {} pair(s) into {}",
                stalled + 1,
                ctx.manifest.entries.len(),
                entry.name,
                harvested.len(),
                rerere::DIR,
            );
        }
        st.results.push(EntryResult {
            name: entry.name.clone(),
            oid,
            status: "merged".into(),
            conflicted: true,
            resolution: Some(rerere::label(&harvested)),
        });
        // Persist the advance IMMEDIATELY: if the very next entry errors, a
        // stale index would re-merge this one and falsely report it EMPTY.
        st.next_index = stalled + 1;
        state::write(&ctx.worktree, &st)?;
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
