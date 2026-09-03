//! Derived entries: a combined branch that merges two topics and builds on
//! both is carried alone, and `build` reconstructs it from its parents rather
//! than merging a pin that is stale the moment either parent moves.

mod common;
use common::*;

use std::fs;

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
