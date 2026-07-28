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
    assert!(out3.contains("verified: reproduced the lock's tree exactly"), "{out3}");
}

#[test]
fn conflict_resolve_continue_then_replay() {
    let fx = fixture();
    topic(&fx, "t1", "b.txt", "one\nT1\nthree\n");
    topic(&fx, "t2", "b.txt", "one\nT2\nthree\n");
    add_branch(&fx, "t1");
    add_branch(&fx, "t2");

    let out = ff_stopped(&fx.root, &["build"]);
    assert!(out.contains("CONFLICT"), "{out}");
    assert!(out.contains("b.txt"), "{out}");

    // Resolve by combining both sides, stage, continue.
    let wt = fx.root.join(".worktrees/build");
    fs::write(wt.join("b.txt"), "one\nT1+T2\nthree\n").unwrap();
    git(&wt, &["add", "b.txt"]);
    let out = ff_ok(&fx.root, &["continue"]);
    assert!(out.contains("resolved; recorded resolutions/t2.patch"), "{out}");

    assert!(fx.root.join("resolutions/t2.toml").exists());
    assert!(fx.root.join("resolutions/t2.patch").exists());
    let lock = lock_json(&fx.root);
    let tree = lock["build"]["tree"].as_str().unwrap().to_string();
    let results = lock["build"]["results"].as_array().unwrap();
    assert_eq!(results[1]["conflicted"], true);

    // Wipe the worktree; locked reproduction must replay the recorded
    // resolution non-interactively and land on the identical tree.
    fs::remove_dir_all(fx.root.join(".worktrees/build")).unwrap();
    let out = ff_ok(&fx.root, &["build", "--locked"]);
    assert!(out.contains("replayed recorded resolution"), "{out}");
    assert!(out.contains("verified: reproduced the lock's tree exactly"), "{out}");
    assert_eq!(lock_json(&fx.root)["build"]["tree"].as_str().unwrap(), tree);
}

#[test]
fn stale_record_proposes_then_rerecords() {
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
    let old_theirs =
        fs::read_to_string(fx.root.join("resolutions/t2.toml")).unwrap();

    // t2 moves: same conflicting hunk, new content. update repins it.
    git(&fx.upstream, &["checkout", "-q", "t2"]);
    fs::write(fx.upstream.join("b.txt"), "one\nT2v2\nthree\n").unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "t2 v2"]);
    git(&fx.upstream, &["checkout", "-q", "main"]);
    let out = ff_ok(&fx.root, &["update", "t2"]);
    assert!(out.contains("t2: "), "{out}");

    // Build stops with the stale record staged as a proposal.
    let out = ff_stopped(&fx.root, &["build"]);
    assert!(out.contains("PROPOSED"), "{out}");

    // The human fixes the proposal to account for v2, stages, continues.
    let wt = fx.root.join(".worktrees/build");
    fs::write(wt.join("b.txt"), "one\nT1+T2v2\nthree\n").unwrap();
    git(&wt, &["add", "b.txt"]);
    let out = ff_ok(&fx.root, &["continue"]);
    assert!(out.contains("resolved; recorded"), "{out}");

    let new_theirs = fs::read_to_string(fx.root.join("resolutions/t2.toml")).unwrap();
    assert_ne!(old_theirs, new_theirs, "sidecar must be rewritten");
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

#[test]
fn absorbed_and_empty_are_flagged() {
    let fx = fixture();
    // absorbed: topic merged into main before the first build.
    topic(&fx, "landed", "c.txt", "landed\n");
    git(&fx.upstream, &["merge", "-q", "--no-ff", "-m", "land it", "landed"]);
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
