//! Integration tests: drive the compiled binary against synthetic git repos.
//! No network — every remote is a local path repository.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_fork-assembler")
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
        .expect("fork-assembler runs")
}

fn ff_ok(dir: &Path, args: &[&str]) -> String {
    let out = ff(dir, args);
    assert!(
        out.status.success(),
        "fork-assembler {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
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
        "fork-assembler {args:?} expected exit 2:\nstdout:\n{}\nstderr:\n{}",
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

/// git that is allowed to fail, for hand-making the conflicted merges a real
/// combined branch contains.
fn git_try(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
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
        .expect("git runs")
        .status
        .success()
}

/// A combined branch as it exists in the wild: it merges each parent in turn
/// and then adds work of its own on top. Passing an empty `file` makes it a
/// pure merge of its parents.
fn combined(fx: &Fixture, name: &str, parents: &[&str], file: &str, content: &str) {
    git(&fx.upstream, &["checkout", "-q", "-b", name, "main"]);
    for parent in parents {
        git(
            &fx.upstream,
            &[
                "merge",
                "-q",
                "--no-ff",
                "-m",
                &format!("merge {parent}"),
                parent,
            ],
        );
    }
    if !file.is_empty() {
        fs::write(fx.upstream.join(file), content).unwrap();
        git(&fx.upstream, &["add", "-A"]);
        git(
            &fx.upstream,
            &["commit", "-q", "-m", &format!("{name} own work")],
        );
    }
    git(&fx.upstream, &["checkout", "-q", "main"]);
}

/// Carry `name` as a derived entry whose parents are the named branches.
fn add_derived(fx: &Fixture, name: &str, parents: &[&str]) {
    let mut args = vec!["add".to_string(), format!("up:{name}")];
    for parent in parents {
        args.push("--parent".into());
        args.push(format!("up:{parent}"));
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    ff_ok(&fx.root, &args);
}

fn publish_derived_to_its_branch(fx: &Fixture, name: &str) {
    let path = fx.root.join("manifest.toml");
    let manifest = fs::read_to_string(&path).unwrap();
    let branch = format!("branch = \"up:{name}\"");
    let replacement = format!("{branch}\nreconstruction_publish = \"up:{name}\"");
    assert!(
        manifest.contains(&branch),
        "missing derived branch {branch:?}"
    );
    fs::write(path, manifest.replacen(&branch, &replacement, 1)).unwrap();
}

fn build_worktree(fx: &Fixture) -> PathBuf {
    fx.root.join(".worktrees/build")
}

fn derive_worktree(fx: &Fixture) -> PathBuf {
    fx.root.join(".worktrees/derive")
}

fn read(path: PathBuf) -> String {
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn append_manifest(root: &Path, text: &str) {
    let path = root.join("manifest.toml");
    let current = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("{current}{text}")).unwrap();
}

/// The stderr of a command that must fail.
fn ff_err(dir: &Path, args: &[&str]) -> String {
    let out = ff(dir, args);
    assert!(
        !out.status.success(),
        "fork-assembler {args:?} unexpectedly succeeded:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stderr).to_string()
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
        .position(|s| s == "fork-assembler: fixup t2")
        .unwrap_or_else(|| panic!("no fixup commit in {log:?}"));
    let t2_at = log
        .iter()
        .position(|s| s == "fork-assembler: merge t2")
        .unwrap();
    let t3_at = log
        .iter()
        .position(|s| s == "fork-assembler: merge t3")
        .unwrap();
    assert!(t2_at < fixup_at && fixup_at < t3_at, "{log:?}");

    // The tree AT the entry boundary is already coherent — not just the final
    // tree. That is the property a trailing patch entry cannot give you.
    let commit = commit_by_subject(&wt, "fork-assembler: fixup t2");
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
        log.iter()
            .filter(|s| *s == "fork-assembler: merge t1")
            .count(),
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
    let fixup_at = log
        .iter()
        .position(|s| s == "fork-assembler: fixup t2")
        .unwrap();
    let t3_at = log
        .iter()
        .position(|s| s == "fork-assembler: merge t3")
        .unwrap();
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
        log.iter()
            .filter(|s| *s == "fork-assembler: merge t1")
            .count(),
        1,
        "the merge must not be replayed: {log:?}"
    );
    assert_eq!(
        log.iter()
            .filter(|s| *s == "fork-assembler: fixup t1")
            .count(),
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
    assert!(out.contains("fork-assembler: fixup t1"), "{out}");
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
    assert!(err.contains("fork-assembler remove t1"), "{err}");
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

/// Derived entries: a combined branch that merges two topics and builds on
/// both is carried alone, and `build` reconstructs it from its parents rather
/// than merging a pin that is stale the moment either parent moves.

#[test]
fn a_derived_entry_is_reconstructed_from_its_parents() {
    let fx = fixture();
    topic(&fx, "a", "c.txt", "from a\n");
    topic(&fx, "b", "d.txt", "from b\n");
    combined(&fx, "c", &["a", "b"], "e.txt", "own work\n");
    add_derived(&fx, "c", &["a", "b"]);

    let out = ff_ok(&fx.root, &["build"]);
    assert!(out.contains("reconstructing from base"), "{out}");
    assert!(out.contains("parent a merged"), "{out}");
    assert!(out.contains("parent b merged"), "{out}");
    assert!(out.contains("delta: 1 commit(s) of its own"), "{out}");

    // The assembled tree carries both parents' content and the entry's own.
    let wt = build_worktree(&fx);
    assert_eq!(read(wt.join("c.txt")), "from a\n");
    assert_eq!(read(wt.join("d.txt")), "from b\n");
    assert_eq!(read(wt.join("e.txt")), "own work\n");

    let lock = lock_json(&fx.root);
    assert!(lock["pins"]["parents"]["c"]["a"].is_string(), "{lock}");
    assert!(lock["pins"]["parents"]["c"]["b"].is_string(), "{lock}");
    // The anchor is the last merge the branch made by hand: everything above
    // it is the entry's own work.
    assert_eq!(
        lock["pins"]["anchors"]["c"].as_str().unwrap(),
        git(&fx.upstream, &["rev-parse", "c^"])
    );
    let result = &lock["build"]["results"][0];
    // The lock still records the PIN as what this entry tracks; the
    // reconstruction is recorded alongside it, not in place of it.
    assert_eq!(
        result["oid"].as_str().unwrap(),
        git(&fx.upstream, &["rev-parse", "c"])
    );
    assert!(result["derived"]["base_tip"].is_string(), "{lock}");
    assert!(result["derived"]["tip"].is_string(), "{lock}");
    // The reconstruction worktree is cleaned once the entry's step is done.
    assert!(!derive_worktree(&fx).exists());

    // Reconstruction is deterministic in trees, which is the invariant this
    // project verifies -- the commits themselves are freshly generated.
    let out = ff_ok(&fx.root, &["build", "--locked"]);
    assert!(
        out.contains("verified: reproduced the lock's tree exactly"),
        "{out}"
    );
}

#[test]
fn a_parent_that_advances_is_rebuilt_under_the_entrys_own_work() {
    let fx = fixture();
    topic(&fx, "a", "c.txt", "from a\n");
    topic(&fx, "b", "d.txt", "from b\n");
    combined(&fx, "c", &["a", "b"], "e.txt", "own work\n");
    add_derived(&fx, "c", &["a", "b"]);
    ff_ok(&fx.root, &["build"]);

    // a gains a commit; the entry's pin does not move.
    git(&fx.upstream, &["checkout", "-q", "a"]);
    fs::write(fx.upstream.join("c.txt"), "from a, then more\n").unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "a moves on"]);
    git(&fx.upstream, &["checkout", "-q", "main"]);

    let out = ff_ok(&fx.root, &["update", "c"]);
    assert!(out.contains("parent a:"), "{out}");
    assert!(out.contains("anchor"), "{out}");

    let out = ff_ok(&fx.root, &["status"]);
    assert!(out.contains("parent pins moved"), "{out}");

    ff_ok(&fx.root, &["build"]);
    let wt = build_worktree(&fx);
    assert_eq!(read(wt.join("c.txt")), "from a, then more\n");
    assert_eq!(read(wt.join("e.txt")), "own work\n");
}

/// The case that rules out `git cherry` and `rev-list C ^A ^B`: once a parent
/// is rebased, the copies of its commits inside C's history match nothing
/// reachable from the new tip, and any patch-id comparison replays them as C's
/// own work -- resurrecting the content the rebase replaced. Anchoring inside
/// C's own history is immune.
#[test]
fn a_rebased_parent_does_not_resurrect_its_old_content() {
    let fx = fixture();
    topic(&fx, "a", "c.txt", "a original\n");
    topic(&fx, "b", "d.txt", "from b\n");
    combined(&fx, "c", &["a", "b"], "e.txt", "own work\n");
    add_derived(&fx, "c", &["a", "b"]);
    ff_ok(&fx.root, &["build"]);

    // a is rewritten in place: same branch point, new content, new OIDs.
    git(&fx.upstream, &["checkout", "-q", "a"]);
    fs::write(fx.upstream.join("c.txt"), "a amended\n").unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "--amend", "-m", "a"]);
    git(&fx.upstream, &["checkout", "-q", "main"]);

    ff_ok(&fx.root, &["update", "c"]);
    let out = ff_ok(&fx.root, &["build"]);
    assert!(out.contains("delta: 1 commit(s) of its own"), "{out}");

    let wt = build_worktree(&fx);
    assert_eq!(read(wt.join("c.txt")), "a amended\n");
    assert_eq!(read(wt.join("e.txt")), "own work\n");
    // The old parent content is gone from the assembled tree entirely.
    assert!(
        !git_try(&wt, &["grep", "-q", "a original", "HEAD"]),
        "the rebased-away content came back"
    );
}

#[test]
fn a_conflict_between_parents_stops_in_the_derive_worktree() {
    let fx = fixture();
    topic(&fx, "a", "b.txt", "one\nA\nthree\n");
    topic(&fx, "b", "b.txt", "one\nB\nthree\n");
    // The combined branch resolved this conflict once, by hand, when it was
    // made; fork-assembler has to be taught the same resolution.
    git(&fx.upstream, &["checkout", "-q", "-b", "c", "main"]);
    git(
        &fx.upstream,
        &["merge", "-q", "--no-ff", "-m", "merge a", "a"],
    );
    assert!(
        !git_try(&fx.upstream, &["merge", "--no-ff", "-m", "merge b", "b"]),
        "the parents were supposed to conflict"
    );
    fs::write(fx.upstream.join("b.txt"), "one\nA+B\nthree\n").unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "merge b"]);
    fs::write(fx.upstream.join("e.txt"), "own work\n").unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "c own work"]);
    git(&fx.upstream, &["checkout", "-q", "main"]);
    add_derived(&fx, "c", &["a", "b"]);

    let out = ff_stopped(&fx.root, &["build"]);
    assert!(out.contains("CONFLICT merging parent b"), "{out}");
    assert!(out.contains("Resolve in the DERIVE worktree"), "{out}");
    assert!(out.contains(".worktrees/derive"), "{out}");
    assert!(out.contains("not the assembled stack"), "{out}");

    // Resolving happens in the derive worktree, not the build one.
    let derive = derive_worktree(&fx);
    fs::write(derive.join("b.txt"), "one\nA+B\nthree\n").unwrap();
    git(&derive, &["add", "b.txt"]);
    let out = ff_ok(&fx.root, &["continue"]);
    assert!(out.contains("harvested 1 pair(s)"), "{out}");

    let pairs = pair_dirs(&fx.root);
    assert_eq!(pairs.len(), 1, "one tracked pair expected");
    let index = fs::read_to_string(fx.root.join("resolutions/rerere/INDEX.toml")).unwrap();
    // Attributed to the entry the manifest carries, not to the parent.
    assert!(index.contains("entry = \"c\""), "{index}");

    let wt = build_worktree(&fx);
    assert_eq!(read(wt.join("b.txt")), "one\nA+B\nthree\n");
    assert_eq!(read(wt.join("e.txt")), "own work\n");
    let tree = lock_json(&fx.root)["build"]["tree"]
        .as_str()
        .unwrap()
        .to_string();

    // From scratch, the tracked pair replays the same resolution during the
    // derive phase and lands on the identical tree.
    fs::remove_dir_all(&wt).unwrap();
    let out = ff_ok(&fx.root, &["build", "--locked"]);
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
fn a_delta_commit_that_conflicts_stops_in_the_derive_worktree() {
    let fx = fixture();
    topic(&fx, "a", "b.txt", "one\nA\nthree\n");
    topic(&fx, "b", "d.txt", "from b\n");
    // The entry's own work edits the line its parent introduced.
    combined(&fx, "c", &["a", "b"], "b.txt", "one\nA-own\nthree\n");
    add_derived(&fx, "c", &["a", "b"]);
    ff_ok(&fx.root, &["build"]);

    // a rewrites that same line: the replay of the entry's own commit now
    // conflicts with the parent it was written against.
    git(&fx.upstream, &["checkout", "-q", "a"]);
    fs::write(fx.upstream.join("b.txt"), "one\nA2\nthree\n").unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "a rewrites its line"]);
    git(&fx.upstream, &["checkout", "-q", "main"]);
    ff_ok(&fx.root, &["update", "c"]);

    let out = ff_stopped(&fx.root, &["build"]);
    assert!(out.contains("CONFLICT replaying"), "{out}");
    assert!(out.contains("Resolve in the DERIVE worktree"), "{out}");

    let derive = derive_worktree(&fx);
    fs::write(derive.join("b.txt"), "one\nA2-own\nthree\n").unwrap();
    git(&derive, &["add", "b.txt"]);
    ff_ok(&fx.root, &["continue"]);

    let wt = build_worktree(&fx);
    assert_eq!(read(wt.join("b.txt")), "one\nA2-own\nthree\n");
    assert_eq!(read(wt.join("d.txt")), "from b\n");
    assert!(!derive_worktree(&fx).exists());
}

/// The operator pushed the reconstruction and the PR then gained review
/// commits. The last build's parent merge is an ancestor of the new pin, so it
/// IS the boundary: everything above it is the entry's own work, and the
/// commits that were replayed to build it are not replayed again.
#[test]
fn pushing_the_reconstruction_moves_the_anchor_to_it() {
    let fx = fixture();
    topic(&fx, "a", "c.txt", "from a\n");
    topic(&fx, "b", "d.txt", "from b\n");
    combined(&fx, "c", &["a", "b"], "e.txt", "own\n");
    add_derived(&fx, "c", &["a", "b"]);
    ff_ok(&fx.root, &["build"]);

    // Publish the reconstructed tip as the branch, the way an operator would.
    let tip = lock_json(&fx.root)["build"]["results"][0]["derived"]["tip"]
        .as_str()
        .unwrap()
        .to_string();
    let source = fx.root.join(".worktrees/source");
    git(
        &source,
        &["push", "-q", "up", &format!("+{tip}:refs/heads/c")],
    );
    // Review lands on top of the published branch.
    git(&fx.upstream, &["checkout", "-q", "c"]);
    fs::write(fx.upstream.join("e.txt"), "own\nreview\n").unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "c review fix"]);
    git(&fx.upstream, &["checkout", "-q", "main"]);

    let out = ff_ok(&fx.root, &["update", "c"]);
    assert!(out.contains("the reconstructed tip was pushed"), "{out}");
    let lock = lock_json(&fx.root);
    assert_eq!(
        lock["pins"]["anchors"]["c"].as_str().unwrap(),
        lock["build"]["results"][0]["derived"]["base_tip"]
            .as_str()
            .unwrap()
    );

    let out = ff_ok(&fx.root, &["build"]);
    assert!(out.contains("delta: 2 commit(s) of its own"), "{out}");
    let wt = build_worktree(&fx);
    // The review commit is in, and the own commit it sits on is in exactly
    // once: replaying it twice would have doubled the line or conflicted.
    assert_eq!(read(wt.join("e.txt")), "own\nreview\n");
    let log = subjects(&wt);
    assert_eq!(
        log.iter().filter(|s| *s == "c own work").count(),
        1,
        "{log:?}"
    );
    assert_eq!(
        log.iter().filter(|s| *s == "c review fix").count(),
        1,
        "{log:?}"
    );
}

#[test]
fn a_derived_entry_can_publish_its_completed_reconstruction() {
    let fx = fixture();
    topic(&fx, "a", "c.txt", "from a\n");
    topic(&fx, "b", "d.txt", "from b\n");
    combined(&fx, "c", &["a", "b"], "e.txt", "own\n");
    add_derived(&fx, "c", &["a", "b"]);
    publish_derived_to_its_branch(&fx, "c");

    let out = ff_ok(&fx.root, &["build"]);
    assert!(out.contains("c published reconstruction"), "{out}");
    let tip = lock_json(&fx.root)["build"]["results"][0]["derived"]["tip"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(git(&fx.upstream, &["rev-parse", "c"]), tip);

    // A locked reproduction rebuilds the same tree but must not write the
    // configured review branch.
    let out = ff_ok(&fx.root, &["build", "--locked"]);
    assert!(
        out.contains("reconstruction publication to up:c skipped (--locked)"),
        "{out}"
    );
    assert_eq!(git(&fx.upstream, &["rev-parse", "c"]), tip);
}

#[test]
fn a_pure_merge_of_its_parents_has_an_empty_delta() {
    let fx = fixture();
    topic(&fx, "a", "c.txt", "from a\n");
    topic(&fx, "b", "d.txt", "from b\n");
    combined(&fx, "c", &["a", "b"], "", "");
    add_derived(&fx, "c", &["a", "b"]);

    let out = ff_ok(&fx.root, &["build"]);
    assert!(out.contains("delta: none -- a pure merge"), "{out}");
    let wt = build_worktree(&fx);
    assert_eq!(read(wt.join("c.txt")), "from a\n");
    assert_eq!(read(wt.join("d.txt")), "from b\n");
    // The tip itself is the boundary: it is a merge, so nothing sits above it.
    assert_eq!(
        lock_json(&fx.root)["pins"]["anchors"]["c"]
            .as_str()
            .unwrap(),
        git(&fx.upstream, &["rev-parse", "c"])
    );
    ff_ok(&fx.root, &["build", "--locked"]);
}

#[test]
fn an_absorbed_derived_entry_is_not_reconstructed() {
    let fx = fixture();
    topic(&fx, "a", "c.txt", "from a\n");
    topic(&fx, "b", "d.txt", "from b\n");
    combined(&fx, "c", &["a", "b"], "e.txt", "own work\n");
    // The whole combination lands upstream.
    git(
        &fx.upstream,
        &["merge", "-q", "--no-ff", "-m", "land c", "c"],
    );
    add_derived(&fx, "c", &["a", "b"]);

    let out = ff_ok(&fx.root, &["build"]);
    assert!(out.contains("ABSORBED upstream"), "{out}");
    assert!(!out.contains("reconstructing"), "{out}");
    assert!(!derive_worktree(&fx).exists());
    assert_eq!(
        lock_json(&fx.root)["build"]["results"][0]["status"],
        "absorbed"
    );
    assert!(lock_json(&fx.root)["build"]["results"][0]["derived"].is_null());
}

/// A parent is carried -- inside the entry that merged it. Every other way of
/// carrying it duplicates those commits, so the manifest refuses to hold both
/// statements at once, exactly as it does for carrying and excluding.
#[test]
fn carrying_a_parent_as_an_entry_too_is_an_error() {
    let fx = fixture();
    topic(&fx, "a", "c.txt", "from a\n");
    combined(&fx, "c", &["a"], "e.txt", "own\n");
    add_branch(&fx, "a");
    append_manifest(
        &fx.root,
        "\n[[entry]]\nbranch = \"up:c\"\nparents = [{ branch = \"up:a\" }]\n",
    );
    let err = ff_err(&fx.root, &["status"]);
    assert!(err.contains("carried both as entry \"a\""), "{err}");
    assert!(err.contains("as a parent of \"c\""), "{err}");
    assert!(err.contains("merges them twice"), "{err}");
}

#[test]
fn excluding_a_parent_is_an_error() {
    let fx = fixture();
    append_manifest(
        &fx.root,
        "\n[[entry]]\nbranch = \"up:c\"\nparents = [{ pr = 7 }]\n\n\
         [[exclude]]\npr = 7\nreason = \"contained in c\"\n",
    );
    let err = ff_err(&fx.root, &["status"]);
    assert!(err.contains("declared as a parent of \"c\""), "{err}");
    assert!(err.contains("delete the exclusion"), "{err}");
    assert!(err.contains("contained in c"), "{err}");
}

#[test]
fn patch_entries_cannot_carry_parents() {
    let fx = fixture();
    append_manifest(
        &fx.root,
        "\n[[entry]]\npatch = \"patches/p.patch\"\nparents = [{ pr = 7 }]\n",
    );
    let err = ff_err(&fx.root, &["status"]);
    assert!(
        err.contains("patch entries cannot carry `parents`"),
        "{err}"
    );
    assert!(err.contains("no history to reconstruct"), "{err}");
}

#[test]
fn a_parent_cannot_be_a_patch() {
    let fx = fixture();
    append_manifest(
        &fx.root,
        "\n[[entry]]\nbranch = \"up:c\"\nparents = [{ patch = \"patches/p.patch\" }]\n",
    );
    let err = ff_err(&fx.root, &["status"]);
    assert!(err.contains("is a patch"), "{err}");
    assert!(err.contains("no commits to re-merge"), "{err}");
}

#[test]
fn add_parent_writes_the_inline_form_and_loads_back() {
    let fx = fixture();
    topic(&fx, "a", "c.txt", "from a\n");
    topic(&fx, "c", "e.txt", "own\n");
    let out = ff_ok(
        &fx.root,
        &["add", "up:c", "--parent", "2525", "--parent", "up:a"],
    );
    assert!(out.contains("parent: pr 2525"), "{out}");
    assert!(out.contains("parent: branch up:a"), "{out}");

    let manifest = fs::read_to_string(fx.root.join("manifest.toml")).unwrap();
    assert!(
        manifest.contains("parents = [{ pr = 2525 }, { branch = \"up:a\" }]"),
        "{manifest}"
    );

    // The typed load reads back exactly what was written, in order.
    let out = ff_ok(&fx.root, &["status"]);
    assert!(out.contains("parent pr-2525"), "{out}");
    assert!(out.contains("parent a"), "{out}");
    assert!(out.contains("UNRESOLVED"), "{out}");

    // And the same target may not then be added as an entry of its own.
    let err = ff_err(&fx.root, &["add", "up:a"]);
    assert!(err.contains("carried as a parent of entry \"c\""), "{err}");
    assert!(err.contains("merge them twice"), "{err}");
}

/// Removing a derived entry takes its parents' commits out of the stack with
/// it, and takes the declaration that kept discovery away from them.
#[test]
fn removing_a_derived_entry_reports_its_parents() {
    let fx = fixture();
    topic(&fx, "a", "c.txt", "from a\n");
    topic(&fx, "b", "d.txt", "from b\n");
    combined(&fx, "c", &["a", "b"], "e.txt", "own\n");
    add_derived(&fx, "c", &["a", "b"]);

    let out = ff_ok(&fx.root, &["remove", "c"]);
    assert!(
        out.contains("declared branch up:a, branch up:b as parent(s)"),
        "{out}"
    );
    assert!(out.contains("will offer them again"), "{out}");
}

/// A parent that the base swallows is dead weight in the declaration: the
/// merge that would bring it in is a no-op, and status says so.
#[test]
fn status_flags_a_parent_the_base_has_absorbed() {
    let fx = fixture();
    topic(&fx, "a", "c.txt", "from a\n");
    topic(&fx, "b", "d.txt", "from b\n");
    combined(&fx, "c", &["a", "b"], "e.txt", "own\n");
    add_derived(&fx, "c", &["a", "b"]);
    ff_ok(&fx.root, &["build"]);

    git(
        &fx.upstream,
        &["merge", "-q", "--no-ff", "-m", "land a", "a"],
    );
    ff_ok(&fx.root, &["update", "base"]);
    let out = ff_ok(&fx.root, &["status"]);
    assert!(
        out.contains("absorbed upstream -- consider removing it"),
        "{out}"
    );

    // The reconstruction still succeeds: an absorbed parent is reported and
    // skipped, not merged as an empty commit.
    let out = ff_ok(&fx.root, &["build"]);
    assert!(out.contains("parent a ABSORBED"), "{out}");
    assert_eq!(read(build_worktree(&fx).join("e.txt")), "own\n");
}

/// The reconstruction can conflict with the stack it is merged into, which is
/// an ordinary entry conflict in the ordinary place. What must survive the stop
/// is the reconstruction itself: `continue` finishes the merge and still has to
/// record what was rebuilt, and the pin as what the entry tracks.
#[test]
fn a_stack_conflict_on_a_derived_entry_resolves_in_the_build_worktree() {
    let fx = fixture();
    topic(&fx, "z", "b.txt", "one\nZ\nthree\n");
    topic(&fx, "a", "c.txt", "from a\n");
    topic(&fx, "b", "d.txt", "from b\n");
    combined(&fx, "c", &["a", "b"], "b.txt", "one\nC\nthree\n");
    add_branch(&fx, "z");
    add_derived(&fx, "c", &["a", "b"]);

    let out = ff_stopped(&fx.root, &["build"]);
    assert!(out.contains("CONFLICT"), "{out}");
    // The reconstruction completed; this stop is in the build worktree.
    assert!(!out.contains("DERIVE worktree"), "{out}");

    let wt = build_worktree(&fx);
    fs::write(wt.join("b.txt"), "one\nZ+C\nthree\n").unwrap();
    git(&wt, &["add", "b.txt"]);
    let out = ff_ok(&fx.root, &["continue"]);
    assert!(out.contains("harvested 1 pair(s)"), "{out}");

    let lock = lock_json(&fx.root);
    let result = &lock["build"]["results"][1];
    assert!(result["derived"]["tip"].is_string(), "{lock}");
    assert_eq!(
        result["oid"].as_str().unwrap(),
        git(&fx.upstream, &["rev-parse", "c"])
    );
    assert_eq!(read(wt.join("b.txt")), "one\nZ+C\nthree\n");
    assert_eq!(read(wt.join("c.txt")), "from a\n");
}

/// A lock written before derived entries existed has no parent pins, no
/// anchor, and no reconstruction record. It must still load, and the build
/// must re-establish what is missing rather than fail on its absence.
#[test]
fn a_lock_without_derived_fields_still_loads() {
    let fx = fixture();
    topic(&fx, "a", "c.txt", "from a\n");
    topic(&fx, "b", "d.txt", "from b\n");
    combined(&fx, "c", &["a", "b"], "e.txt", "own work\n");
    add_derived(&fx, "c", &["a", "b"]);
    ff_ok(&fx.root, &["build"]);

    let mut lock = lock_json(&fx.root);
    let pins = lock["pins"].as_object_mut().unwrap();
    pins.remove("parents");
    pins.remove("anchors");
    for result in lock["build"]["results"].as_array_mut().unwrap() {
        result.as_object_mut().unwrap().remove("derived");
    }
    for snapshot in lock["build"]["manifest_entries"].as_array_mut().unwrap() {
        let snapshot = snapshot.as_object_mut().unwrap();
        snapshot.remove("parents");
        snapshot.remove("anchor");
    }
    fs::write(
        fx.root.join("manifest.lock.json"),
        serde_json::to_string_pretty(&lock).unwrap(),
    )
    .unwrap();

    let out = ff_ok(&fx.root, &["build"]);
    assert!(out.contains("pinned c's parent a"), "{out}");
    assert!(out.contains("anchor"), "{out}");
    assert_eq!(read(build_worktree(&fx).join("e.txt")), "own work\n");
    let lock = lock_json(&fx.root);
    assert!(lock["pins"]["anchors"]["c"].is_string(), "{lock}");
}

/// A delta commit whose change the parent has since made itself replays to
/// nothing. That is a skip, not a failure: the content is in, and the entry
/// keeps whatever else it owns.
#[test]
fn a_delta_commit_the_parent_now_contains_is_skipped() {
    let fx = fixture();
    topic(&fx, "a", "c.txt", "from a\n");
    git(&fx.upstream, &["checkout", "-q", "-b", "c", "main"]);
    git(
        &fx.upstream,
        &["merge", "-q", "--no-ff", "-m", "merge a", "a"],
    );
    fs::write(fx.upstream.join("d.txt"), "shared fix\n").unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "c own work"]);
    git(&fx.upstream, &["checkout", "-q", "main"]);
    add_derived(&fx, "c", &["a"]);
    ff_ok(&fx.root, &["build"]);

    // The parent adopts the same fix, verbatim.
    git(&fx.upstream, &["checkout", "-q", "a"]);
    fs::write(fx.upstream.join("d.txt"), "shared fix\n").unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "a adopts the fix"]);
    git(&fx.upstream, &["checkout", "-q", "main"]);
    ff_ok(&fx.root, &["update", "c"]);

    let out = ff_ok(&fx.root, &["build"]);
    assert!(out.contains("EMPTY -- already present"), "{out}");
    let wt = build_worktree(&fx);
    assert_eq!(read(wt.join("d.txt")), "shared fix\n");
    assert_eq!(read(wt.join("c.txt")), "from a\n");
    ff_ok(&fx.root, &["build", "--locked"]);
}

/// Two entries may declare the same parent: each reconstruction is standalone,
/// and the parent's content arrives twice as identical trees, which merge
/// cleanly. Only carrying a parent AS an entry duplicates commits.
#[test]
fn two_entries_may_share_a_parent() {
    let fx = fixture();
    topic(&fx, "a", "c.txt", "from a\n");
    combined(&fx, "c1", &["a"], "e1.txt", "one\n");
    combined(&fx, "c2", &["a"], "e2.txt", "two\n");
    add_derived(&fx, "c1", &["a"]);
    add_derived(&fx, "c2", &["a"]);

    ff_ok(&fx.root, &["build"]);
    let wt = build_worktree(&fx);
    assert_eq!(read(wt.join("c.txt")), "from a\n");
    assert_eq!(read(wt.join("e1.txt")), "one\n");
    assert_eq!(read(wt.join("e2.txt")), "two\n");
    let lock = lock_json(&fx.root);
    assert!(lock["pins"]["parents"]["c1"]["a"].is_string(), "{lock}");
    assert!(lock["pins"]["parents"]["c2"]["a"].is_string(), "{lock}");
    ff_ok(&fx.root, &["build", "--locked"]);
}

/// Parents take the pr shape too, fetched from `refs/pull/N/head` on the base
/// remote exactly as a pr entry is -- the case the combined-PR story is
/// actually about.
#[test]
fn a_pr_parent_is_fetched_and_reconstructed() {
    let fx = fixture();
    topic(&fx, "a", "c.txt", "from a\n");
    // Publish topic a as PR 7 the way a forge would.
    let head = git(&fx.upstream, &["rev-parse", "a"]);
    git(&fx.upstream, &["update-ref", "refs/pull/7/head", &head]);
    combined(&fx, "c", &["a"], "e.txt", "own work\n");
    ff_ok(&fx.root, &["add", "up:c", "--parent", "7"]);

    let out = ff_ok(&fx.root, &["build"]);
    assert!(out.contains("pinned c's parent pr-7"), "{out}");
    assert!(out.contains("parent pr-7 merged"), "{out}");
    let wt = build_worktree(&fx);
    assert_eq!(read(wt.join("c.txt")), "from a\n");
    assert_eq!(read(wt.join("e.txt")), "own work\n");
    assert_eq!(
        lock_json(&fx.root)["pins"]["parents"]["c"]["pr-7"]
            .as_str()
            .unwrap(),
        head
    );
    ff_ok(&fx.root, &["build", "--locked"]);
}

/// `build` consumes the recorded anchor and never re-derives it -- the same
/// rule that keeps it from moving a pin. That is also what makes a bad
/// detection correctable: the anchor is a lock field, and editing it changes
/// exactly which commits the next build replays.
#[test]
fn build_replays_from_the_recorded_anchor_without_re_detecting() {
    let fx = fixture();
    topic(&fx, "a", "c.txt", "from a\n");
    git(&fx.upstream, &["checkout", "-q", "-b", "c", "main"]);
    git(
        &fx.upstream,
        &["merge", "-q", "--no-ff", "-m", "merge a", "a"],
    );
    for own in ["own1", "own2"] {
        fs::write(fx.upstream.join(format!("{own}.txt")), format!("{own}\n")).unwrap();
        git(&fx.upstream, &["add", "-A"]);
        git(&fx.upstream, &["commit", "-q", "-m", own]);
    }
    git(&fx.upstream, &["checkout", "-q", "main"]);
    add_derived(&fx, "c", &["a"]);

    let out = ff_ok(&fx.root, &["build"]);
    assert!(out.contains("delta: 2 commit(s)"), "{out}");
    let wt = build_worktree(&fx);
    assert_eq!(read(wt.join("own1.txt")), "own1\n");
    assert_eq!(read(wt.join("own2.txt")), "own2\n");

    // Move the boundary up by one commit, as an operator correcting a
    // detection would.
    let mut lock = lock_json(&fx.root);
    lock["pins"]["anchors"]["c"] =
        serde_json::Value::String(git(&fx.upstream, &["rev-parse", "c^"]));
    fs::write(
        fx.root.join("manifest.lock.json"),
        serde_json::to_string_pretty(&lock).unwrap(),
    )
    .unwrap();

    let out = ff_ok(&fx.root, &["status"]);
    assert!(out.contains("anchor moved"), "{out}");
    let out = ff_ok(&fx.root, &["build"]);
    assert!(out.contains("delta: 1 commit(s)"), "{out}");
    assert!(!wt.join("own1.txt").exists(), "the delta was not honored");
    assert_eq!(read(wt.join("own2.txt")), "own2\n");
}

/// A removed entry's pin is not a fact about anything, and a stale one is
/// actively misleading once the same ref reappears as a parent pinned
/// elsewhere. The next build drops it; a re-added entry pins fresh.
#[test]
fn removing_an_entry_drops_its_pin_from_the_lock() {
    let fx = fixture();
    topic(&fx, "a", "c.txt", "from a\n");
    topic(&fx, "b", "d.txt", "from b\n");
    add_branch(&fx, "a");
    add_branch(&fx, "b");
    ff_ok(&fx.root, &["build"]);
    assert!(lock_json(&fx.root)["pins"]["entries"]["a"].is_string());

    ff_ok(&fx.root, &["remove", "a"]);
    ff_ok(&fx.root, &["build"]);
    let lock = lock_json(&fx.root);
    assert!(lock["pins"]["entries"]["a"].is_null(), "{lock}");
    assert!(lock["pins"]["entries"]["b"].is_string(), "{lock}");
}

/// Move upstream main on, so a topic branched before it goes stale.
fn advance_main(fx: &Fixture, file: &str, content: &str) {
    fs::write(fx.upstream.join(file), content).unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "upstream moves on"]);
}

/// A topic that cannot merge with the base ALONE is out of date with upstream,
/// which is a fact about the topic and not about this assembly. Refuse it
/// outright rather than offering a resolution: a tracked pair would hide the
/// breakage from the topic's own author and come back on every base bump.
#[test]
fn a_topic_conflicting_with_the_base_is_refused() {
    let fx = fixture();
    topic(&fx, "stale", "b.txt", "one\nSTALE\nthree\n");
    advance_main(&fx, "b.txt", "one\nUPSTREAM\nthree\n");
    add_branch(&fx, "stale");

    let out = ff(&fx.root, &["build"]);
    // A hard error, not the exit-2 "stopped for resolution" contract.
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("conflicts with the BASE ITSELF"), "{err}");
    assert!(err.contains("b.txt"), "{err}");
    assert!(err.contains("rebase"), "{err}");

    // Nothing was recorded: the resolution belongs on the topic branch.
    assert!(pair_dirs(&fx.root).is_empty());
    assert!(!fx.root.join("manifest.lock.json").exists());

    // And nothing is left in progress, so the next build reports the same
    // refusal rather than "a build is already in progress".
    let again = String::from_utf8_lossy(&ff(&fx.root, &["build"]).stderr).to_string();
    assert!(again.contains("conflicts with the BASE ITSELF"), "{again}");
}

/// The refusal fires the moment the merge conflicts, before rerere gets a
/// chance to replay a pair over it -- otherwise a base conflict recorded once
/// would keep silently applying to every later upstream head.
#[test]
fn a_tracked_pair_cannot_paper_over_a_base_conflict() {
    let fx = fixture();
    topic(&fx, "t1", "b.txt", "one\nT1\nthree\n");
    topic(&fx, "t2", "b.txt", "one\nT2\nthree\n");
    add_branch(&fx, "t1");
    add_branch(&fx, "t2");

    // An ordinary cross-topic conflict, resolved and harvested as usual.
    ff_stopped(&fx.root, &["build"]);
    fs::write(build_worktree(&fx).join("b.txt"), "one\nT1+T2\nthree\n").unwrap();
    git(&build_worktree(&fx), &["add", "b.txt"]);
    ff_ok(&fx.root, &["continue"]);
    assert_eq!(pair_dirs(&fx.root).len(), 1);

    // Upstream now takes the same line. t2's pair still matches the conflict,
    // but the conflict is with the base now, so it must not be replayed.
    advance_main(&fx, "b.txt", "one\nUPSTREAM\nthree\n");
    ff_ok(&fx.root, &["update", "base"]);
    let err = ff_err(&fx.root, &["build"]);
    assert!(err.contains("conflicts with the BASE ITSELF"), "{err}");
}

/// The same fact, reported before a build has to stop for it.
#[test]
fn status_flags_a_topic_that_conflicts_with_the_base() {
    let fx = fixture();
    topic(&fx, "clean", "c.txt", "from clean\n");
    topic(&fx, "stale", "b.txt", "one\nSTALE\nthree\n");
    add_branch(&fx, "clean");
    ff_ok(&fx.root, &["build"]);

    advance_main(&fx, "b.txt", "one\nUPSTREAM\nthree\n");
    ff_ok(&fx.root, &["update", "base"]);
    add_branch(&fx, "stale");
    ff_ok(&fx.root, &["update", "stale"]);

    let out = ff_ok(&fx.root, &["status"]);
    let stale = out
        .lines()
        .find(|l| l.contains("stale"))
        .unwrap_or_default();
    assert!(stale.contains("CONFLICTS WITH BASE"), "{out}");
    let clean = out
        .lines()
        .find(|l| l.trim_start().starts_with("clean"))
        .unwrap_or_default();
    assert!(!clean.contains("CONFLICTS WITH BASE"), "{out}");
}
