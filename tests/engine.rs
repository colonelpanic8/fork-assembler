//! Integration tests: drive the compiled binary against synthetic git repos.
//! No network — every remote is a local path repository.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_fork-fold")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed in {}:\n{}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn ff(dir: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("fork-fold runs")
}

fn ff_ok(dir: &Path, args: &[&str]) -> String {
    let out = ff(dir, args);
    assert!(
        out.status.success(),
        "fork-fold {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn ff_stopped(dir: &Path, args: &[&str]) -> String {
    let out = ff(dir, args);
    assert_eq!(
        out.status.code(),
        Some(2),
        "fork-fold {args:?} expected exit 2:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

struct Fixture {
    _tmp: TempDir,
    upstream: PathBuf,
    root: PathBuf,
}

/// An upstream repo with a base commit on main, plus a maintenance root whose
/// manifest points at it. Topics are branches created in the upstream repo.
fn fixture() -> Fixture {
    let tmp = TempDir::new().expect("tempdir");
    let upstream = tmp.path().join("upstream");
    fs::create_dir_all(&upstream).unwrap();
    git(&upstream, &["init", "-q", "-b", "main"]);
    fs::write(upstream.join("a.txt"), "base\n").unwrap();
    fs::write(upstream.join("b.txt"), "one\ntwo\nthree\n").unwrap();
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-q", "-m", "base"]);

    let root = tmp.path().join("stack");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("manifest.toml"),
        format!(
            "[remotes]\nup = \"{}\"\n\n[base]\nremote = \"up\"\nref = \"main\"\n",
            upstream.display()
        ),
    )
    .unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    Fixture {
        _tmp: tmp,
        upstream,
        root,
    }
}

/// Create a topic branch off main in the upstream repo with one commit
/// writing `content` to `file`, then return to main.
fn topic(fx: &Fixture, name: &str, file: &str, content: &str) {
    git(&fx.upstream, &["checkout", "-q", "-b", name, "main"]);
    fs::write(fx.upstream.join(file), content).unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", name]);
    git(&fx.upstream, &["checkout", "-q", "main"]);
}

fn add_branch(fx: &Fixture, name: &str) {
    ff_ok(&fx.root, &["add", &format!("up:{name}")]);
}

fn lock_json(root: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(root.join("manifest.lock.json")).unwrap()).unwrap()
}

#[test]
fn clean_build_then_noop() {
    let fx = fixture();
    topic(&fx, "t1", "c.txt", "from t1\n");
    topic(&fx, "t2", "d.txt", "from t2\n");
    add_branch(&fx, "t1");
    add_branch(&fx, "t2");

    let out = ff_ok(&fx.root, &["build"]);
    assert!(out.contains("merged"), "{out}");
    let lock = lock_json(&fx.root);
    let tree = lock["build"]["tree"].as_str().unwrap().to_string();
    assert!(!tree.is_empty());
    assert_eq!(lock["build"]["results"].as_array().unwrap().len(), 2);

    // Second build: pins unchanged, manifest unchanged -> up to date, no work.
    let out2 = ff_ok(&fx.root, &["build"]);
    assert!(out2.contains("up to date"), "{out2}");
    assert_eq!(lock_json(&fx.root)["build"]["tree"].as_str().unwrap(), tree);

    // Locked reproduction from pins rebuilds and verifies the same tree.
    let out3 = ff_ok(&fx.root, &["build", "--locked"]);
    assert!(
        out3.contains("verified: reproduced the lock's tree exactly"),
        "{out3}"
    );
}

/// Hash directories under resolutions/rerere/ (tracked pair entries).
fn pair_dirs(root: &Path) -> Vec<PathBuf> {
    let dir = root.join("resolutions/rerere");
    if !dir.exists() {
        return Vec::new();
    }
    let mut dirs: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().unwrap().is_dir())
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

#[test]
fn conflict_harvest_then_rebuild_autoresolves() {
    let fx = fixture();
    topic(&fx, "t1", "b.txt", "one\nT1\nthree\n");
    topic(&fx, "t2", "b.txt", "one\nT2\nthree\n");
    add_branch(&fx, "t1");
    add_branch(&fx, "t2");

    let out = ff_stopped(&fx.root, &["build"]);
    assert!(out.contains("CONFLICT"), "{out}");
    assert!(out.contains("b.txt"), "{out}");

    // Resolve by combining both sides, stage, continue: the resolved
    // conflict is harvested as a tracked rerere pair.
    let wt = fx.root.join(".worktrees/build");
    fs::write(wt.join("b.txt"), "one\nT1+T2\nthree\n").unwrap();
    git(&wt, &["add", "b.txt"]);
    let out = ff_ok(&fx.root, &["continue"]);
    assert!(out.contains("harvested 1 pair(s)"), "{out}");

    let pairs = pair_dirs(&fx.root);
    assert_eq!(pairs.len(), 1, "one tracked pair expected");
    assert!(pairs[0].join("preimage").exists());
    assert!(pairs[0].join("postimage").exists());
    let index = fs::read_to_string(fx.root.join("resolutions/rerere/INDEX.toml")).unwrap();
    assert!(index.contains("entry = \"t2\""), "{index}");
    assert!(index.contains("b.txt"), "{index}");

    let lock = lock_json(&fx.root);
    let tree = lock["build"]["tree"].as_str().unwrap().to_string();
    let results = lock["build"]["results"].as_array().unwrap();
    assert_eq!(results[1]["conflicted"], true);
    assert!(results[1]["resolution"]
        .as_str()
        .unwrap()
        .starts_with("rerere:"));

    // Wipe the worktree; locked reproduction must auto-resolve from the
    // seeded tracked pairs and land on the identical tree.
    fs::remove_dir_all(fx.root.join(".worktrees/build")).unwrap();
    let out = ff_ok(&fx.root, &["build", "--locked"]);
    assert!(out.contains("seeded 1 tracked rerere pair(s)"), "{out}");
    assert!(
        out.contains("auto-resolved from tracked rerere pairs"),
        "{out}"
    );
    assert!(
        out.contains("verified: reproduced the lock's tree exactly"),
        "{out}"
    );
    assert_eq!(lock_json(&fx.root)["build"]["tree"].as_str().unwrap(), tree);
}

#[test]
fn drift_outside_conflict_hunks_still_autoresolves() {
    let fx = fixture();
    // A file long enough that a change at the top stays out of the
    // conflict hunk's context at the bottom.
    let base = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\n";
    fs::write(fx.upstream.join("e.txt"), base).unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "add e.txt"]);

    topic(&fx, "t1", "e.txt", &base.replace("l8", "T1"));
    topic(&fx, "t2", "e.txt", &base.replace("l8", "T2"));
    add_branch(&fx, "t1");
    add_branch(&fx, "t2");

    ff_stopped(&fx.root, &["build"]);
    let wt = fx.root.join(".worktrees/build");
    fs::write(wt.join("e.txt"), base.replace("l8", "T1+T2")).unwrap();
    git(&wt, &["add", "e.txt"]);
    let out = ff_ok(&fx.root, &["continue"]);
    assert!(out.contains("harvested 1 pair(s)"), "{out}");

    // t0 changes the same file far from the conflict, plus another file.
    let drifted = base.replace("l1", "L1-drift");
    topic(&fx, "t0", "e.txt", &drifted);
    // Reorder so t0 lands BEFORE the conflicting pair: trees differ from the
    // recorded build, but the conflict hunks are identical.
    ff_ok(&fx.root, &["remove", "t1"]);
    ff_ok(&fx.root, &["remove", "t2"]);
    add_branch(&fx, "t0");
    add_branch(&fx, "t1");
    add_branch(&fx, "t2");

    let out = ff_ok(&fx.root, &["build"]);
    assert!(
        out.contains("auto-resolved from tracked rerere pairs"),
        "{out}"
    );
    let body = fs::read_to_string(wt.join("e.txt")).unwrap();
    assert!(body.contains("L1-drift"), "{body}");
    assert!(body.contains("T1+T2"), "{body}");
}

#[test]
fn changed_conflict_hunks_fall_back_to_manual() {
    let fx = fixture();
    topic(&fx, "t1", "b.txt", "one\nT1\nthree\n");
    topic(&fx, "t2", "b.txt", "one\nT2\nthree\n");
    add_branch(&fx, "t1");
    add_branch(&fx, "t2");

    ff_stopped(&fx.root, &["build"]);
    let wt = fx.root.join(".worktrees/build");
    fs::write(wt.join("b.txt"), "one\nT1+T2\nthree\n").unwrap();
    git(&wt, &["add", "b.txt"]);
    ff_ok(&fx.root, &["continue"]);
    assert_eq!(pair_dirs(&fx.root).len(), 1);

    // t2 moves: the conflict hunk itself changes, so the tracked pair no
    // longer matches and the build must stop for manual resolution.
    git(&fx.upstream, &["checkout", "-q", "t2"]);
    fs::write(fx.upstream.join("b.txt"), "one\nT2v2\nthree\n").unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "t2 v2"]);
    git(&fx.upstream, &["checkout", "-q", "main"]);
    ff_ok(&fx.root, &["update", "t2"]);

    let out = ff_stopped(&fx.root, &["build"]);
    assert!(out.contains("CONFLICT"), "{out}");
    assert!(!out.contains("auto-resolved"), "{out}");

    fs::write(wt.join("b.txt"), "one\nT1+T2v2\nthree\n").unwrap();
    git(&wt, &["add", "b.txt"]);
    let out = ff_ok(&fx.root, &["continue"]);
    assert!(out.contains("harvested 1 pair(s)"), "{out}");
    // The new conflict hashes differently: both pairs are now tracked.
    assert_eq!(pair_dirs(&fx.root).len(), 2);
}

#[test]
fn append_extends_incrementally() {
    let fx = fixture();
    topic(&fx, "t1", "c.txt", "from t1\n");
    add_branch(&fx, "t1");
    ff_ok(&fx.root, &["build"]);
    let old_pre = lock_json(&fx.root)["build"]["pre_provenance_commit"]
        .as_str()
        .unwrap()
        .to_string();

    topic(&fx, "t2", "d.txt", "from t2\n");
    add_branch(&fx, "t2");
    let out = ff_ok(&fx.root, &["build"]);
    assert!(out.contains("extending the locked build"), "{out}");

    // The prefix's commits are reused verbatim: the new merge's first parent
    // is exactly the previous build's head.
    let new_pre = lock_json(&fx.root)["build"]["pre_provenance_commit"]
        .as_str()
        .unwrap()
        .to_string();
    let wt = fx.root.join(".worktrees/build");
    let parent = git(&wt, &["rev-parse", &format!("{new_pre}^1")]);
    assert_eq!(parent, old_pre);
}

#[test]
fn patch_entry_applies_and_detects_already_applied() {
    let fx = fixture();
    // A patch that appends a line to a.txt, generated from a scratch commit.
    git(&fx.upstream, &["checkout", "-q", "-b", "scratch", "main"]);
    fs::write(fx.upstream.join("a.txt"), "base\npatched\n").unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "scratch"]);
    let patch = git(&fx.upstream, &["format-patch", "--stdout", "-1", "HEAD"]);
    git(&fx.upstream, &["checkout", "-q", "main"]);

    fs::create_dir_all(fx.root.join("patches")).unwrap();
    fs::write(fx.root.join("patches/append-a.patch"), patch + "\n").unwrap();
    ff_ok(&fx.root, &["add", "--patch", "patches/append-a.patch"]);
    let out = ff_ok(&fx.root, &["build"]);
    assert!(out.contains("applied"), "{out}");
    let lock = lock_json(&fx.root);
    assert_eq!(lock["build"]["results"][0]["status"], "applied");

    // Same change lands on a topic ahead of the patch: the patch must be
    // detected as already applied, not double-applied or errored.
    topic(&fx, "dup", "a.txt", "base\npatched\n");
    let root = &fx.root;
    // Insert the branch entry BEFORE the patch entry by rewriting the
    // manifest: remove and re-add in the right order.
    ff_ok(root, &["remove", "append-a"]);
    add_branch(&fx, "dup");
    ff_ok(root, &["add", "--patch", "patches/append-a.patch"]);
    let out = ff_ok(root, &["build"]);
    assert!(out.contains("already applied"), "{out}");
}

/// A plain unified diff rewriting `file` to `content`, taken against `branch`
/// so it applies at that branch's position in the stack.
fn patch_against(fx: &Fixture, branch: &str, file: &str, content: &str) -> String {
    git(
        &fx.upstream,
        &["checkout", "-q", "-b", "scratch-patch", branch],
    );
    fs::write(fx.upstream.join(file), content).unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "scratch"]);
    let diff = git(&fx.upstream, &["diff", branch, "HEAD"]);
    git(&fx.upstream, &["checkout", "-q", "main"]);
    git(&fx.upstream, &["branch", "-q", "-D", "scratch-patch"]);
    diff + "\n"
}

fn write_patch(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// Subjects of the assembled branch, oldest first.
fn subjects(worktree: &Path) -> Vec<String> {
    git(worktree, &["log", "--format=%s"])
        .lines()
        .rev()
        .map(str::to_string)
        .collect()
}

/// The commit whose subject is exactly `subject`.
fn commit_by_subject(worktree: &Path, subject: &str) -> String {
    git(worktree, &["log", "--format=%H%x1f%s"])
        .lines()
        .find_map(|line| line.split_once('\x1f').filter(|(_, s)| *s == subject))
        .unwrap_or_else(|| panic!("no commit titled {subject:?}"))
        .0
        .to_string()
}

/// The whole point of attaching a fixup to an entry: it lands inside that
/// entry's step, so the boundary between entries is never an invalid tree.
#[test]
fn fixup_applies_within_its_entry_step() {
    let fx = fixture();
    topic(&fx, "t1", "c.txt", "from t1\n");
    topic(&fx, "t2", "d.txt", "claims-42\n");
    topic(&fx, "t3", "e.txt", "from t3\n");
    // t2 collides with t1 on a resource no single branch owns; the repair
    // belongs to admitting t2, not to a patch entry at the end of the stack.
    write_patch(
        &fx.root,
        "patches/renumber.patch",
        &patch_against(&fx, "t2", "d.txt", "claims-43\n"),
    );

    add_branch(&fx, "t1");
    add_branch(&fx, "t2");
    add_branch(&fx, "t3");
    let out = ff_ok(&fx.root, &["fixup", "t2", "patches/renumber.patch"]);
    assert!(out.contains("coherence fixup set to"), "{out}");
    let out = ff_ok(&fx.root, &["build"]);
    assert!(
        out.contains("fixup patches/renumber.patch: applied"),
        "{out}"
    );

    let wt = fx.root.join(".worktrees/build");
    let log = subjects(&wt);
    let fixup_at = log
        .iter()
        .position(|s| s == "fork-fold: fixup t2")
        .unwrap_or_else(|| panic!("no fixup commit in {log:?}"));
    let t2_at = log.iter().position(|s| s == "fork-fold: merge t2").unwrap();
    let t3_at = log.iter().position(|s| s == "fork-fold: merge t3").unwrap();
    assert!(t2_at < fixup_at && fixup_at < t3_at, "{log:?}");

    // The tree AT the entry boundary is already coherent — not just the final
    // tree. That is the property a trailing patch entry cannot give you.
    let commit = commit_by_subject(&wt, "fork-fold: fixup t2");
    let at_boundary = git(&wt, &["show", &format!("{commit}:d.txt")]);
    assert_eq!(at_boundary, "claims-43");

    let lock = lock_json(&fx.root);
    assert!(lock["build"]["results"][1]["fixup"].is_string());
    assert!(lock["build"]["manifest_entries"][1]["fixup"].is_string());
    assert!(lock["build"]["results"][0]["fixup"].is_null());

    ff_ok(&fx.root, &["build", "--locked"]);
}

/// Editing a fixup must invalidate exactly like repinning its entry does —
/// no `update` step, since a fixup is repo-local content, not a remote ref.
#[test]
fn editing_a_fixup_invalidates_the_build() {
    let fx = fixture();
    topic(&fx, "t1", "d.txt", "claims-42\n");
    write_patch(
        &fx.root,
        "patches/renumber.patch",
        &patch_against(&fx, "t1", "d.txt", "claims-43\n"),
    );
    add_branch(&fx, "t1");
    ff_ok(&fx.root, &["fixup", "t1", "patches/renumber.patch"]);
    ff_ok(&fx.root, &["build"]);
    let first = lock_json(&fx.root)["build"]["tree"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(ff_ok(&fx.root, &["build"]).contains("up to date"));

    write_patch(
        &fx.root,
        "patches/renumber.patch",
        &patch_against(&fx, "t1", "d.txt", "claims-44\n"),
    );
    let out = ff_ok(&fx.root, &["status"]);
    assert!(out.contains("fixup patches/renumber.patch"), "{out}");
    assert!(out.contains("(t1)'s fixup changed"), "{out}");

    let out = ff_ok(&fx.root, &["build"]);
    assert!(
        out.contains("fixup patches/renumber.patch: applied"),
        "{out}"
    );
    let second = lock_json(&fx.root)["build"]["tree"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(first, second, "an edited fixup must change the built tree");
    let wt = fx.root.join(".worktrees/build");
    assert_eq!(fs::read_to_string(wt.join("d.txt")).unwrap(), "claims-44\n");
}

/// A fixup that no longer applies stops the build without re-running the
/// merge, and `--capture` turns the human's repair back into the patch file.
#[test]
fn failed_fixup_stops_then_capture_repairs() {
    let fx = fixture();
    topic(&fx, "t1", "c.txt", "from t1\n");
    // Patches a file that does not exist at t1's boundary: a stale fixup.
    write_patch(
        &fx.root,
        "patches/coherence.patch",
        &patch_against(&fx, "main", "a.txt", "base\nvia fixup\n").replace("a.txt", "missing.txt"),
    );
    add_branch(&fx, "t1");
    ff_ok(&fx.root, &["fixup", "t1", "patches/coherence.patch"]);

    let out = ff_stopped(&fx.root, &["build"]);
    assert!(
        out.contains("fixup patches/coherence.patch FAILED"),
        "{out}"
    );
    assert!(out.contains("The merge is committed"), "{out}");

    // The merge IS committed and must not be redone; only the fixup is owed.
    let wt = fx.root.join(".worktrees/build");
    let log = subjects(&wt);
    assert_eq!(
        log.iter().filter(|s| *s == "fork-fold: merge t1").count(),
        1,
        "{log:?}"
    );

    // Repair in the worktree, then capture the corrected fixup from it.
    fs::write(wt.join("a.txt"), "base\nvia fixup\n").unwrap();
    let out = ff_ok(
        &fx.root,
        &["fixup", "t1", "patches/coherence.patch", "--capture"],
    );
    assert!(out.contains("uncommitted changes"), "{out}");

    let out = ff_ok(&fx.root, &["build"]);
    assert!(
        out.contains("fixup patches/coherence.patch: applied"),
        "{out}"
    );
    assert_eq!(
        fs::read_to_string(wt.join("a.txt")).unwrap(),
        "base\nvia fixup\n"
    );
    ff_ok(&fx.root, &["build", "--locked"]);
}

/// A fixup repairs an interaction between entries, so removing one side must
/// surface the detached patch rather than silently dropping it.
#[test]
fn removing_an_entry_reports_its_orphaned_fixup() {
    let fx = fixture();
    topic(&fx, "t1", "d.txt", "claims-42\n");
    write_patch(
        &fx.root,
        "patches/renumber.patch",
        &patch_against(&fx, "t1", "d.txt", "claims-43\n"),
    );
    add_branch(&fx, "t1");
    ff_ok(&fx.root, &["fixup", "t1", "patches/renumber.patch"]);

    let out = ff_ok(&fx.root, &["remove", "t1"]);
    assert!(out.contains("carried the coherence fixup"), "{out}");
    assert!(out.contains("patches/renumber.patch"), "{out}");
    // Left on disk: the incoherence it repaired may well outlive the entry.
    assert!(fx.root.join("patches/renumber.patch").exists());
}

/// A manual conflict resolution and the entry's fixup are ONE step:
/// `continue` must commit the resolution and then run the fixup, leaving the
/// entry boundary coherent before the next entry merges.
#[test]
fn continue_runs_the_fixup_after_a_manual_resolution() {
    let fx = fixture();
    topic(&fx, "t1", "b.txt", "one\nT1\nthree\n");
    topic(&fx, "t2", "b.txt", "one\nT2\nthree\n");
    topic(&fx, "t3", "e.txt", "from t3\n");
    write_patch(
        &fx.root,
        "patches/coherence.patch",
        &patch_against(&fx, "main", "a.txt", "base\ncoherent\n"),
    );
    add_branch(&fx, "t1");
    add_branch(&fx, "t2");
    add_branch(&fx, "t3");
    ff_ok(&fx.root, &["fixup", "t2", "patches/coherence.patch"]);

    ff_stopped(&fx.root, &["build"]);
    let wt = fx.root.join(".worktrees/build");
    fs::write(wt.join("b.txt"), "one\nT1+T2\nthree\n").unwrap();
    git(&wt, &["add", "b.txt"]);
    let out = ff_ok(&fx.root, &["continue"]);
    assert!(out.contains("harvested 1 pair(s)"), "{out}");
    assert!(
        out.contains("fixup patches/coherence.patch: applied"),
        "{out}"
    );

    let log = subjects(&wt);
    let fixup_at = log.iter().position(|s| s == "fork-fold: fixup t2").unwrap();
    let t3_at = log.iter().position(|s| s == "fork-fold: merge t3").unwrap();
    assert!(fixup_at < t3_at, "{log:?}");
    assert!(lock_json(&fx.root)["build"]["results"][1]["fixup"].is_string());
}

/// Resuming from a fixup stall commits only the fixup — the merge already
/// happened — and warns that the patch file no longer matches what shipped.
#[test]
fn continue_resumes_a_stalled_fixup_without_remerging() {
    let fx = fixture();
    topic(&fx, "t1", "c.txt", "from t1\n");
    write_patch(
        &fx.root,
        "patches/coherence.patch",
        &patch_against(&fx, "main", "a.txt", "base\nvia fixup\n").replace("a.txt", "missing.txt"),
    );
    add_branch(&fx, "t1");
    ff_ok(&fx.root, &["fixup", "t1", "patches/coherence.patch"]);
    ff_stopped(&fx.root, &["build"]);

    let wt = fx.root.join(".worktrees/build");
    fs::write(wt.join("a.txt"), "base\nby hand\n").unwrap();
    git(&wt, &["add", "-A"]);
    let out = ff_ok(&fx.root, &["continue"]);
    assert!(out.contains("fixup committed as resolved"), "{out}");
    assert!(out.contains("WARNING"), "{out}");

    let log = subjects(&wt);
    assert_eq!(
        log.iter().filter(|s| *s == "fork-fold: merge t1").count(),
        1,
        "the merge must not be replayed: {log:?}"
    );
    assert_eq!(
        log.iter().filter(|s| *s == "fork-fold: fixup t1").count(),
        1
    );
    assert_eq!(
        fs::read_to_string(wt.join("a.txt")).unwrap(),
        "base\nby hand\n"
    );
    assert!(lock_json(&fx.root)["build"]["results"][0]["fixup"].is_string());

    // The warning is accurate: the tracked inputs no longer reproduce the
    // lock unaided, because the patch file still holds the failing version.
    fs::remove_dir_all(&wt).unwrap();
    let out = ff_stopped(&fx.root, &["build", "--locked"]);
    assert!(
        out.contains("fixup patches/coherence.patch FAILED"),
        "{out}"
    );

    // Re-capturing from the fixup commit is the durable repair.
    fs::write(wt.join("a.txt"), "base\nby hand\n").unwrap();
    git(&wt, &["add", "-A"]);
    ff_ok(&fx.root, &["continue"]);
    let out = ff_ok(
        &fx.root,
        &["fixup", "t1", "patches/coherence.patch", "--capture"],
    );
    assert!(out.contains("fork-fold: fixup t1"), "{out}");
    let out = ff_ok(&fx.root, &["build"]);
    assert!(
        out.contains("fixup patches/coherence.patch: applied"),
        "{out}"
    );
    ff_ok(&fx.root, &["build", "--locked"]);
}

#[test]
fn patch_entries_cannot_carry_a_fixup() {
    let fx = fixture();
    write_patch(&fx.root, "patches/p.patch", "");
    write_patch(&fx.root, "patches/f.patch", "");
    ff_ok(&fx.root, &["add", "--patch", "patches/p.patch"]);
    let out = ff(&fx.root, &["fixup", "p", "patches/f.patch"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("patch entries cannot carry a fixup"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn absorbed_and_empty_are_flagged() {
    let fx = fixture();
    // absorbed: topic merged into main before the first build.
    topic(&fx, "landed", "c.txt", "landed\n");
    git(
        &fx.upstream,
        &["merge", "-q", "--no-ff", "-m", "land it", "landed"],
    );
    // empty: a change plus its exact revert -> tree identical to base.
    git(&fx.upstream, &["checkout", "-q", "-b", "noop", "main"]);
    fs::write(fx.upstream.join("a.txt"), "changed\n").unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "change"]);
    git(&fx.upstream, &["revert", "--no-edit", "HEAD"]);
    git(&fx.upstream, &["checkout", "-q", "main"]);

    add_branch(&fx, "landed");
    add_branch(&fx, "noop");
    let out = ff_ok(&fx.root, &["build"]);
    assert!(out.contains("ABSORBED"), "{out}");
    assert!(out.contains("EMPTY"), "{out}");
    let lock = lock_json(&fx.root);
    assert_eq!(lock["build"]["results"][0]["status"], "absorbed");
    assert_eq!(lock["build"]["results"][1]["status"], "empty");

    // status reports the prune candidate; prune --dry-run agrees.
    let out = ff_ok(&fx.root, &["status"]);
    assert!(out.contains("prune candidate"), "{out}");
    let out = ff_ok(&fx.root, &["prune", "--dry-run"]);
    assert!(out.contains("landed"), "{out}");
}

#[test]
fn provenance_commit_and_tree_invariant() {
    let fx = fixture();
    topic(&fx, "t1", "c.txt", "from t1\n");
    let manifest = fs::read_to_string(fx.root.join("manifest.toml")).unwrap();
    fs::write(
        fx.root.join("manifest.toml"),
        // Top-level key: must precede the first table header.
        format!("provenance_file = \"stack-build-info.json\"\n{manifest}"),
    )
    .unwrap();
    add_branch(&fx, "t1");
    ff_ok(&fx.root, &["build"]);

    let lock = lock_json(&fx.root);
    let build = &lock["build"];
    assert_ne!(build["tree"], build["built_tree"]);
    let wt = fx.root.join(".worktrees/build");
    // The provenance file exists at the head but not in the content tree.
    let head_tree = git(&wt, &["rev-parse", "HEAD^{tree}"]);
    assert_eq!(head_tree, build["built_tree"].as_str().unwrap());
    let pre_tree = git(&wt, &["rev-parse", "HEAD^^{tree}"]);
    assert_eq!(pre_tree, build["tree"].as_str().unwrap());
    let listing = git(&wt, &["ls-tree", "--name-only", "HEAD"]);
    assert!(listing.contains("stack-build-info.json"));
    let body = fs::read_to_string(wt.join("stack-build-info.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["schemaVersion"], 1);
    assert_eq!(json["entries"][0]["label"], "t1");
}

/// An exclusion is the only statement about a non-carried target that
/// survives a discovery sweep, so these cover the three ways it must bite:
/// discovery skips it, an explicit add refuses, and carrying it is incoherent.

#[test]
fn excluding_records_the_reason_and_is_idempotent() {
    let fx = fixture();
    let out = ff_ok(
        &fx.root,
        &["exclude", "--pr", "7", "--reason", "superseded by 9"],
    );
    assert!(out.contains("excluded pr 7"), "{out}");
    assert!(out.contains("nothing needs rebuilding"), "{out}");

    let again = ff_ok(&fx.root, &["exclude", "--pr", "7"]);
    assert!(again.contains("already excluded: pr 7"), "{again}");

    let manifest = fs::read_to_string(fx.root.join("manifest.toml")).unwrap();
    assert_eq!(manifest.matches("[[exclude]]").count(), 1, "{manifest}");
    assert!(manifest.contains("superseded by 9"), "{manifest}");
}

#[test]
fn re_excluding_with_a_new_reason_replaces_it() {
    let fx = fixture();
    ff_ok(&fx.root, &["exclude", "--pr", "7", "--reason", "first"]);
    let out = ff_ok(&fx.root, &["exclude", "--pr", "7", "--reason", "second"]);
    assert!(out.contains("reason updated (was: first)"), "{out}");
    let manifest = fs::read_to_string(fx.root.join("manifest.toml")).unwrap();
    assert_eq!(manifest.matches("[[exclude]]").count(), 1, "{manifest}");
    assert!(!manifest.contains("first"), "{manifest}");
    assert!(manifest.contains("second"), "{manifest}");
}

#[test]
fn adding_an_excluded_target_is_refused() {
    let fx = fixture();
    ff_ok(
        &fx.root,
        &["exclude", "--pr", "7", "--reason", "superseded by 9"],
    );
    let out = ff(&fx.root, &["add", "--pr", "7"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("pr 7 is excluded"), "{err}");
    assert!(err.contains("superseded by 9"), "{err}");

    // The refusal must not have half-written the manifest: the `pr = 7` still
    // in it belongs to the [[exclude]] table, not to a new entry.
    let manifest = fs::read_to_string(fx.root.join("manifest.toml")).unwrap();
    assert!(!manifest.contains("[[entry]]"), "{manifest}");
}

#[test]
fn excluding_a_carried_target_defers_to_remove() {
    let fx = fixture();
    topic(&fx, "t1", "c.txt", "one\n");
    ff_ok(&fx.root, &["add", "up:t1"]);
    let out = ff(&fx.root, &["exclude", "up:t1"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("carried by entry"), "{err}");
    assert!(err.contains("fork-fold remove t1"), "{err}");
}

#[test]
fn carrying_and_excluding_the_same_target_is_an_error() {
    let fx = fixture();
    topic(&fx, "t1", "c.txt", "one\n");
    ff_ok(&fx.root, &["add", "up:t1"]);
    // Hand-edit past the `exclude` verb's guard, the way a bad merge would.
    let path = fx.root.join("manifest.toml");
    let manifest = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        format!("{manifest}\n[[exclude]]\nbranch = \"up:t1\"\nreason = \"stale\"\n"),
    )
    .unwrap();

    let out = ff(&fx.root, &["status"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("both carries and excludes"), "{err}");
    assert!(err.contains("delete whichever is wrong"), "{err}");
}

#[test]
fn status_lists_exclusions_with_their_reasons() {
    let fx = fixture();
    ff_ok(
        &fx.root,
        &["exclude", "--pr", "7", "--reason", "superseded"],
    );
    ff_ok(&fx.root, &["exclude", "--pr", "8"]);
    let out = ff_ok(&fx.root, &["status"]);
    assert!(out.contains("excluded:"), "{out}");
    assert!(out.contains("pr 7 (superseded)"), "{out}");
    assert!(out.contains("pr 8 (no reason recorded)"), "{out}");
}

#[test]
fn an_exclusion_names_exactly_one_target() {
    let fx = fixture();
    let path = fx.root.join("manifest.toml");
    let manifest = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        format!("{manifest}\n[[exclude]]\npr = 7\npatch = \"patches/p.patch\"\n"),
    )
    .unwrap();
    let out = ff(&fx.root, &["status"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("must name exactly one"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
