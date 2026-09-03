//! Coherence fixups: a tracked patch applied inside its entry's own step.

mod common;
use common::*;

use std::fs;

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
