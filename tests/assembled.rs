//! `assembled` and `diff`: the assembled branch of any maintenance commit is
//! at hand — regenerated when its object is gone — and two of them can be
//! diffed.

mod common;
use common::*;

use std::fs;
use std::path::Path;

fn commit_recipe(root: &Path, message: &str) -> String {
    fs::write(root.join(".gitignore"), ".worktrees/\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", message]);
    git(root, &["rev-parse", "HEAD"])
}

/// The assembled commit the lock at maintenance commit `rev` records.
fn built_at(root: &Path, rev: &str) -> String {
    let lock = git(root, &["show", &format!("{rev}:manifest.lock.json")]);
    let lock: serde_json::Value = serde_json::from_str(&lock).unwrap();
    lock["build"]["commit"].as_str().unwrap().to_string()
}

fn clone_elsewhere(fx: &Fixture) -> std::path::PathBuf {
    git(
        fx._tmp.path(),
        &["clone", "-q", &fx.root.to_string_lossy(), "elsewhere"],
    );
    fx._tmp.path().join("elsewhere")
}

/// `assembled REV`'s stdout, which is the commit and nothing else.
fn assembled(root: &Path, rev: &str) -> String {
    let out = ff(root, &["assembled", rev]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Three maintenance commits: one entry; a second whose merge was resolved by
/// hand; then the first repinned.
fn three_commits(fx: &Fixture) -> Vec<String> {
    topic(fx, "t1", "b.txt", "one\nT1\nthree\n");
    topic(fx, "t2", "b.txt", "one\nT2\nthree\n");
    add_branch(fx, "t1");
    ff_ok(&fx.root, &["build"]);
    let first = commit_recipe(&fx.root, "carry t1");

    add_branch(fx, "t2");
    ff_stopped(&fx.root, &["build"]);
    let wt = build_worktree(fx);
    fs::write(wt.join("b.txt"), "one\nT1+T2\nthree\n").unwrap();
    git(&wt, &["add", "b.txt"]);
    ff_ok(&fx.root, &["continue"]);
    let second = commit_recipe(&fx.root, "carry t2");

    git(&fx.upstream, &["checkout", "-q", "t1"]);
    fs::write(fx.upstream.join("c.txt"), "t1 grows\n").unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "t1 grows"]);
    git(&fx.upstream, &["checkout", "-q", "main"]);
    ff_ok(&fx.root, &["update", "t1"]);
    ff_ok(&fx.root, &["build"]);
    let third = commit_recipe(&fx.root, "repin t1");
    vec![first, second, third]
}

#[test]
fn assembled_prints_what_a_maintenance_commit_built() {
    let fx = fixture();
    let revs = three_commits(&fx);
    for rev in &revs {
        assert_eq!(assembled(&fx.root, rev), built_at(&fx.root, rev));
    }
    assert_eq!(assembled(&fx.root, "HEAD~1"), built_at(&fx.root, &revs[1]));
}

/// A clone of the maintenance repository has none of the assembled commits,
/// only recipes. Each one regenerates to exactly what its lock says, and can be
/// checked out to be inspected or built.
#[test]
fn assembled_regenerates_a_historical_build() {
    let fx = fixture();
    let revs = three_commits(&fx);
    let elsewhere = clone_elsewhere(&fx);

    let out = ff(&elsewhere, &["assembled", &revs[1]]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("regenerating"), "{stderr}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        built_at(&fx.root, &revs[1])
    );
    // The regeneration left nothing behind in either repository.
    assert!(!elsewhere.join(".worktrees/build").exists());
    assert_eq!(git(&elsewhere, &["worktree", "list"]).lines().count(), 1);

    let out = ff(&elsewhere, &["assembled", &revs[1]]);
    assert!(!String::from_utf8_lossy(&out.stderr).contains("regenerating"));

    let dir = fx._tmp.path().join("second");
    let out = ff_ok(
        &elsewhere,
        &["assembled", &revs[1], "--checkout", &dir.to_string_lossy()],
    );
    assert!(out.contains("checked out at"), "{out}");
    assert_eq!(read(dir.join("b.txt")), "one\nT1+T2\nthree\n");
    assert_eq!(
        git(&dir, &["rev-parse", "HEAD"]),
        built_at(&fx.root, &revs[1])
    );
}

#[test]
fn a_build_from_before_deterministic_commits_cannot_be_regenerated() {
    let fx = fixture();
    topic(&fx, "t1", "c.txt", "from t1\n");
    add_branch(&fx, "t1");
    ff_ok(&fx.root, &["build"]);
    let path = fx.root.join("manifest.lock.json");
    let mut lock = lock_json(&fx.root);
    lock["build"].as_object_mut().unwrap().remove("protocol");
    fs::write(&path, serde_json::to_string_pretty(&lock).unwrap()).unwrap();
    commit_recipe(&fx.root, "old build");

    // Still present where it was built...
    assert_eq!(assembled(&fx.root, "HEAD"), built_at(&fx.root, "HEAD"));
    // ...but gone for good anywhere else.
    let err = ff_err(&clone_elsewhere(&fx), &["assembled"]);
    assert!(err.contains("predates deterministic commits"), "{err}");
}

#[test]
fn diff_compares_the_assembled_branches() {
    let fx = fixture();
    let path = fx.root.join("manifest.toml");
    let manifest = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        format!("provenance_file = \"stack-build-info.json\"\n{manifest}"),
    )
    .unwrap();
    topic(&fx, "t1", "c.txt", "from t1\n");
    topic(&fx, "t2", "d.txt", "from t2\n");
    add_branch(&fx, "t1");
    add_branch(&fx, "t2");
    ff_ok(&fx.root, &["build"]);
    let before = commit_recipe(&fx.root, "carry t1 and t2");

    advance_main(&fx, "a.txt", "upstream moved\n");
    git(&fx.upstream, &["checkout", "-q", "t1"]);
    fs::write(fx.upstream.join("c.txt"), "from t1, revised\n").unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "t1 revised"]);
    git(&fx.upstream, &["checkout", "-q", "main"]);
    topic(&fx, "t3", "e.txt", "from t3\n");
    ff_ok(&fx.root, &["remove", "t2"]);
    add_branch(&fx, "t3");
    ff_ok(&fx.root, &["update"]);
    ff_ok(&fx.root, &["build"]);

    // Against the working tree, before the new recipe is committed.
    let out = ff_ok(&fx.root, &["diff"]);
    assert!(out.contains("+from t1, revised"), "{out}");
    assert!(out.contains("+upstream moved"), "{out}");
    // The provenance record is metadata about the build, not the stack.
    assert!(!out.contains("stack-build-info"), "{out}");
    let after = commit_recipe(&fx.root, "drop t2, carry t3, bump");

    let out = ff_ok(&fx.root, &["diff", &before, &after, "--carried"]);
    assert!(out.contains("+from t1, revised"), "{out}");
    assert!(out.contains("deleted file mode"), "{out}");
    assert!(out.contains("+from t3"), "{out}");
    assert!(!out.contains("upstream moved"), "{out}");

    let out = ff_ok(&fx.root, &["diff", &before, &after, "--", "--name-status"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines,
        ["M\ta.txt", "M\tc.txt", "D\td.txt", "A\te.txt"],
        "{out}"
    );
    let out = ff_ok(
        &fx.root,
        &["diff", &before, &after, "--", "--stat", "--", "e.txt"],
    );
    assert!(out.contains("e.txt | 1 +"), "{out}");

    let out = ff_ok(&fx.root, &["diff", &after, &after]);
    assert!(out.trim().is_empty(), "{out}");

    // A clone has neither assembled branch; both regenerate.
    let elsewhere = clone_elsewhere(&fx);
    let out = ff(&elsewhere, &["diff", &before, &after, "--", "--name-only"]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stderr.matches("regenerating").count(), 2, "{stderr}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).lines().count(),
        4,
        "{stderr}"
    );
}
