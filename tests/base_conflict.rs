//! The base-conflict refusal: a topic that cannot merge with the base alone
//! is out of date with upstream, and `build` refuses it rather than resolving.

mod common;
use common::*;

use std::fs;

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
