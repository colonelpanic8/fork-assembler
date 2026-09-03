//! The build loop: clean builds, tracked resolutions, incremental extension,
//! patch entries, absorbed and empty merges, provenance, and the lock.

mod common;
use common::*;

use std::fs;

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
