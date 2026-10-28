//! `gc`: removing tracked rerere pairs the lock's last build did not use.

mod common;
use common::*;

use std::fs;

/// Resolve the stopped build's b.txt conflict as `resolution` and finish it.
fn resolved_conflict(fx: &Fixture, resolution: &str) {
    let wt = build_worktree(fx);
    fs::write(wt.join("b.txt"), resolution).unwrap();
    git(&wt, &["add", "b.txt"]);
    ff_ok(&fx.root, &["continue"]);
}

#[test]
fn gc_removes_the_pair_a_changed_conflict_superseded() {
    let fx = fixture();
    topic(&fx, "t1", "b.txt", "one\nT1\nthree\n");
    topic(&fx, "t2", "b.txt", "one\nT2\nthree\n");
    add_branch(&fx, "t1");
    add_branch(&fx, "t2");
    ff_stopped(&fx.root, &["build"]);
    resolved_conflict(&fx, "one\nT1+T2\nthree\n");
    let old = pair_hashes(&fx.root).remove(0);

    git(&fx.upstream, &["checkout", "-q", "t2"]);
    fs::write(fx.upstream.join("b.txt"), "one\nT2v2\nthree\n").unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "t2 v2"]);
    git(&fx.upstream, &["checkout", "-q", "main"]);
    ff_ok(&fx.root, &["update", "t2"]);
    ff_stopped(&fx.root, &["build"]);
    resolved_conflict(&fx, "one\nT1+T2v2\nthree\n");
    let current: Vec<String> = pair_hashes(&fx.root)
        .into_iter()
        .filter(|hash| *hash != old)
        .collect();
    assert_eq!(current.len(), 1);

    let out = ff_ok(&fx.root, &["--format", "json", "gc", "--dry-run"]);
    let report: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(report["verb"], "gc");
    assert_eq!(
        report["data"]["unused"],
        serde_json::json!([{ "hash": old, "entry": "t2" }])
    );
    assert_eq!(report["data"]["kept"], 1);
    assert_eq!(pair_hashes(&fx.root).len(), 2, "a dry run removes nothing");

    let out = ff_ok(&fx.root, &["gc"]);
    assert!(out.contains(&format!("{old} (t2): unused")), "{out}");
    assert!(out.contains("removed 1 unused pair(s)"), "{out}");
    assert_eq!(pair_hashes(&fx.root), current);
    let index = fs::read_to_string(fx.root.join("resolutions/rerere/INDEX.toml")).unwrap();
    assert!(!index.contains(&old), "{index}");
    assert!(index.contains(&current[0]), "{index}");

    fs::remove_dir_all(build_worktree(&fx)).unwrap();
    let out = ff_ok(&fx.root, &["build", "--locked"]);
    assert!(
        out.contains("verified: reproduced the lock's tree exactly"),
        "{out}"
    );
    let out = ff_ok(&fx.root, &["gc"]);
    assert!(out.contains("nothing to collect"), "{out}");
}

#[test]
fn gc_refuses_without_a_record_of_used_pairs() {
    let fx = fixture();
    topic(&fx, "t1", "b.txt", "one\nT1\nthree\n");
    topic(&fx, "t2", "b.txt", "one\nT2\nthree\n");
    add_branch(&fx, "t1");
    add_branch(&fx, "t2");

    let err = ff_err(&fx.root, &["gc"]);
    assert!(err.contains("no completed build"), "{err}");

    ff_stopped(&fx.root, &["build"]);
    let err = ff_err(&fx.root, &["gc"]);
    assert!(err.contains("a build is in progress"), "{err}");

    resolved_conflict(&fx, "one\nT1+T2\nthree\n");
    // The shape a lock from before `resolutions` existed has.
    let mut lock = lock_json(&fx.root);
    for result in lock["build"]["results"].as_array_mut().unwrap() {
        let result = result.as_object_mut().unwrap();
        result.remove("resolutions");
        if result["conflicted"] == true {
            result.insert("resolution".into(), "rerere".into());
        }
    }
    fs::write(
        fx.root.join("manifest.lock.json"),
        serde_json::to_string_pretty(&lock).unwrap(),
    )
    .unwrap();

    let err = ff_err(&fx.root, &["gc"]);
    assert!(
        err.contains("t1's result in the lock does not record which rerere pairs it used"),
        "{err}"
    );
    assert_eq!(pair_hashes(&fx.root).len(), 1);

    let before = lock_json(&fx.root);
    let out = ff_ok(&fx.root, &["build", "--locked"]);
    assert!(
        out.contains("recorded the rerere pairs the reproduction used"),
        "{out}"
    );
    let after = lock_json(&fx.root);
    assert_eq!(after["build"]["commit"], before["build"]["commit"]);
    let pair = pair_hashes(&fx.root).remove(0);
    assert_eq!(
        after["build"]["results"][1]["resolutions"],
        serde_json::json!([pair])
    );
    let out = ff_ok(&fx.root, &["gc"]);
    assert!(out.contains("nothing to collect"), "{out}");

    let out = ff_ok(&fx.root, &["build", "--locked"]);
    assert!(
        !out.contains("recorded the rerere pairs"),
        "an up-to-date record is not rewritten: {out}"
    );
}
