//! An exclusion is the only statement about a non-carried target that
//! survives a discovery sweep, so these cover the three ways it must bite:
//! discovery skips it, an explicit add refuses, and carrying it is incoherent.

mod common;
use common::*;

use std::fs;

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
