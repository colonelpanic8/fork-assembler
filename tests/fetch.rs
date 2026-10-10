//! Live refs are fetched in one `git fetch` per remote, and a batch that git
//! aborts still reports which entry's ref broke it.

mod common;
use common::*;

use std::fs;
use std::process::Command;

/// Run fork-assembler with git tracing on; return how many `git fetch`
/// processes it ran and the command's stderr.
fn traced(fx: &Fixture, args: &[&str]) -> (usize, String) {
    let trace = fx._tmp.path().join("trace");
    let _ = fs::remove_file(&trace);
    let out = Command::new(bin())
        .current_dir(&fx.root)
        .args(args)
        .env("GIT_TRACE", &trace)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("fork-assembler runs");
    let fetches = fs::read_to_string(&trace)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains("built-in: git fetch"))
        .count();
    (fetches, String::from_utf8_lossy(&out.stderr).to_string())
}

/// Three branch entries and a derived entry with two parents, spread over
/// two remotes that both point at the upstream repo.
fn stack() -> Fixture {
    let fx = fixture();
    let manifest = fx.root.join("manifest.toml");
    let text = fs::read_to_string(&manifest).unwrap().replacen(
        "[remotes]\n",
        &format!("[remotes]\nfork = \"{}\"\n", fx.upstream.display()),
        1,
    );
    fs::write(&manifest, text).unwrap();

    topic(&fx, "t1", "c.txt", "from t1\n");
    topic(&fx, "t2", "d.txt", "from t2\n");
    topic(&fx, "t3", "e.txt", "from t3\n");
    topic(&fx, "p1", "g.txt", "from p1\n");
    topic(&fx, "p2", "h.txt", "from p2\n");
    combined(&fx, "both", &["p1", "p2"], "f.txt", "own\n");
    add_branch(&fx, "t1");
    ff_ok(&fx.root, &["add", "fork:t2"]);
    ff_ok(&fx.root, &["add", "fork:t3"]);
    add_derived(&fx, "both", &["p1", "p2"]);
    fx
}

#[test]
fn every_verb_fetches_once_per_remote() {
    let fx = stack();

    let (fetches, err) = traced(&fx, &["build"]);
    assert_eq!(fetches, 2, "{err}");
    let (fetches, err) = traced(&fx, &["status", "--live"]);
    assert_eq!(fetches, 2, "{err}");
    let (fetches, err) = traced(&fx, &["update"]);
    assert_eq!(fetches, 2, "{err}");

    // A selective update fetches only the named entry's remote.
    let (fetches, err) = traced(&fx, &["update", "t3"]);
    assert_eq!(fetches, 1, "{err}");

    // Everything is pinned and present: build has nothing to fetch.
    let (fetches, err) = traced(&fx, &["build"]);
    assert_eq!(fetches, 0, "{err}");
}

#[test]
fn a_failed_batch_names_the_missing_ref() {
    let fx = stack();
    ff_ok(&fx.root, &["build"]);
    git(&fx.upstream, &["branch", "-q", "-D", "t3"]);

    let err = ff_err(&fx.root, &["update"]);
    assert!(err.contains("t3: fetching fork refs/heads/t3"), "{err}");
    assert!(
        err.contains("couldn't find remote ref refs/heads/t3"),
        "{err}"
    );

    let err = ff_err(&fx.root, &["status", "--live"]);
    assert!(err.contains("t3: fetching fork refs/heads/t3"), "{err}");
}
