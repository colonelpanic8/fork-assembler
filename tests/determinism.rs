//! Generated commits are a pure function of the recipe: the manifest, the
//! lock's pins, the tracked rerere pairs and the patches. Who builds it, when,
//! where, under what git configuration, and by which route — clean,
//! incremental, or through a human resolving conflicts — must not change a
//! single OID.

mod common;
use common::*;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const PROVENANCE: &str = "stack-build-info.json";

fn record_provenance(root: &Path) {
    let path = root.join("manifest.toml");
    let manifest = fs::read_to_string(&path).unwrap();
    fs::write(
        path,
        format!("provenance_file = \"{PROVENANCE}\"\n{manifest}"),
    )
    .unwrap();
}

/// Commit `content` to `file` on `branch` in the upstream repo, dated `epoch`.
fn commit_dated(fx: &Fixture, branch: &str, file: &str, content: &str, epoch: u64) {
    git(&fx.upstream, &["checkout", "-q", branch]);
    fs::write(fx.upstream.join(file), content).unwrap();
    git(&fx.upstream, &["add", "-A"]);
    let date = format!("@{epoch} +0000");
    let out = Command::new("git")
        .arg("-C")
        .arg(&fx.upstream)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args(["-c", "commit.gpgSign=false", "commit", "-q", "-m", "dated"])
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    git(&fx.upstream, &["checkout", "-q", "main"]);
}

/// Every resolution path the engine has, in one stack: a conflicted stack
/// merge, a derived entry whose parents conflict, a derived entry whose own
/// commit conflicts on replay, a coherence fixup, a patch entry, and a
/// provenance commit. Each conflict is resolved by hand, the way an operator
/// would, so the lock it leaves records commits a human produced.
fn hand_resolved_stack(fx: &Fixture) {
    advance_main(fx, "f.txt", "one\ntwo\nthree\n");
    advance_main(fx, "g.txt", "one\ntwo\nthree\n");
    record_provenance(&fx.root);

    topic(fx, "t1", "b.txt", "one\nT1\nthree\n");
    topic(fx, "t2", "b.txt", "one\nT2\nthree\n");

    topic(fx, "pa", "f.txt", "one\nPA\nthree\n");
    topic(fx, "pb", "f.txt", "one\nPB\nthree\n");
    git(&fx.upstream, &["checkout", "-q", "-b", "c1", "main"]);
    git(
        &fx.upstream,
        &["merge", "-q", "--no-ff", "-m", "merge pa", "pa"],
    );
    assert!(!git_try(
        &fx.upstream,
        &["merge", "--no-ff", "-m", "merge pb", "pb"]
    ));
    fs::write(fx.upstream.join("f.txt"), "one\nPA+PB\nthree\n").unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "merge pb"]);
    fs::write(fx.upstream.join("e.txt"), "c1 own work\n").unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "c1 own work"]);
    git(&fx.upstream, &["checkout", "-q", "main"]);

    topic(fx, "qa", "g.txt", "one\nQA\nthree\n");
    topic(fx, "qb", "h.txt", "from qb\n");
    combined(fx, "c2", &["qa", "qb"], "g.txt", "one\nQA-own\nthree\n");

    add_branch(fx, "t1");
    let fix = patch_against(fx, "t1", "fix.txt", "fixed\n");
    write_patch(&fx.root, "patches/t1-fix.patch", &fix);
    ff_ok(&fx.root, &["fixup", "t1", "patches/t1-fix.patch"]);
    add_branch(fx, "t2");
    add_derived(fx, "c1", &["pa", "pb"]);
    add_derived(fx, "c2", &["qa", "qb"]);
    // Trailing whitespace, so a `git apply` that "fixes" whitespace would
    // change the tree.
    let extra = patch_against(fx, "main", "a.txt", "base\nextra  \nmore\n");
    write_patch(&fx.root, "patches/extra.patch", &extra);
    ff_ok(&fx.root, &["add", "--patch", "patches/extra.patch"]);

    let out = ff_stopped(&fx.root, &["build"]);
    assert!(out.contains("CONFLICT"), "{out}");
    let wt = build_worktree(fx);
    fs::write(wt.join("b.txt"), "one\nT1+T2\nthree\n").unwrap();
    git(&wt, &["add", "b.txt"]);
    let out = ff_stopped(&fx.root, &["continue"]);
    assert!(out.contains("CONFLICT merging parent pb"), "{out}");
    let derive = derive_worktree(fx);
    fs::write(derive.join("f.txt"), "one\nPA+PB\nthree\n").unwrap();
    git(&derive, &["add", "f.txt"]);
    ff_ok(&fx.root, &["continue"]);

    // qa rewrites the line c2's own commit edits: replaying it now conflicts.
    git(&fx.upstream, &["checkout", "-q", "qa"]);
    fs::write(fx.upstream.join("g.txt"), "one\nQA2\nthree\n").unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(
        &fx.upstream,
        &["commit", "-q", "-m", "qa rewrites its line"],
    );
    git(&fx.upstream, &["checkout", "-q", "main"]);
    ff_ok(&fx.root, &["update", "c2"]);
    let out = ff_stopped(&fx.root, &["build"]);
    assert!(out.contains("CONFLICT replaying"), "{out}");
    fs::write(derive.join("g.txt"), "one\nQA2-own\nthree\n").unwrap();
    git(&derive, &["add", "g.txt"]);
    ff_ok(&fx.root, &["continue"]);

    assert_eq!(read(wt.join("b.txt")), "one\nT1+T2\nthree\n");
    assert_eq!(read(wt.join("f.txt")), "one\nPA+PB\nthree\n");
    assert_eq!(read(wt.join("g.txt")), "one\nQA2-own\nthree\n");
    assert_eq!(read(wt.join("fix.txt")), "fixed\n");
    assert_eq!(read(wt.join("a.txt")), "base\nextra  \nmore\n");
    assert!(wt.join(PROVENANCE).exists());
}

/// A machine that disagrees with the first one about everything a commit could
/// pick up from its surroundings.
fn hostile_env(dir: &Path) -> Vec<(&'static str, String)> {
    let config = dir.join("hostile.gitconfig");
    fs::write(
        &config,
        "[user]\n\tname = someone else\n\temail = else@example.invalid\n\
         [merge]\n\tlog = true\n\tconflictStyle = merge\n\trenames = false\n\
         [pull]\n\ttwohead = resolve\n\
         [commit]\n\tcleanup = verbatim\n\
         [i18n]\n\tcommitEncoding = ISO-8859-1\n\
         [apply]\n\twhitespace = fix\n\
         [core]\n\tcommentChar = %\n\tfileMode = false\n\
         [diff]\n\talgorithm = patience\n",
    )
    .unwrap();
    vec![
        ("GIT_CONFIG_GLOBAL", config.to_string_lossy().to_string()),
        ("GIT_CONFIG_COUNT", "1".into()),
        ("GIT_CONFIG_KEY_0", "merge.log".into()),
        ("GIT_CONFIG_VALUE_0", "true".into()),
        ("GIT_AUTHOR_NAME", "someone else".into()),
        ("GIT_AUTHOR_EMAIL", "else@example.invalid".into()),
        ("GIT_AUTHOR_DATE", "@1000000000 +0530".into()),
        ("GIT_COMMITTER_NAME", "someone else".into()),
        ("GIT_COMMITTER_EMAIL", "else@example.invalid".into()),
        ("GIT_COMMITTER_DATE", "@1000000000 +0530".into()),
        ("TZ", "Asia/Kolkata".into()),
    ]
}

fn ff_hostile(dir: &Path, args: &[&str], env: &[(&'static str, String)]) -> String {
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let out = ff_env(dir, args, &env);
    assert!(
        out.status.success(),
        "fork-assembler {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn derived_tips(root: &Path) -> Vec<serde_json::Value> {
    lock_json(root)["build"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["derived"].clone())
        .collect()
}

#[test]
fn the_same_recipe_builds_the_same_commit_anywhere() {
    let fx = fixture();
    hand_resolved_stack(&fx);
    let commit = locked_commit(&fx.root);
    assert_eq!(git(&build_worktree(&fx), &["rev-parse", "HEAD"]), commit);

    // The hand-resolved commits are exactly what replaying the harvested
    // pairs from scratch produces.
    fs::remove_dir_all(build_worktree(&fx)).unwrap();
    let out = ff_ok(&fx.root, &["build", "--locked"]);
    assert!(
        out.contains(&format!("as the identical commit {}", &commit[..12])),
        "{out}"
    );

    // Another repository holding only the tracked recipe, with its own object
    // store, built later, as someone else, under hostile git configuration.
    let elsewhere = fx._tmp.path().join("elsewhere");
    copy_recipe(&fx.root, &elsewhere);
    let env = hostile_env(fx._tmp.path());
    std::thread::sleep(Duration::from_millis(1100));
    ff_hostile(&elsewhere, &["build"], &env);

    assert_eq!(locked_commit(&elsewhere), commit);
    assert_eq!(
        git(&elsewhere.join(".worktrees/build"), &["rev-parse", "HEAD"]),
        commit
    );
    assert_eq!(derived_tips(&elsewhere), derived_tips(&fx.root));
    let out = ff_hostile(&elsewhere, &["build", "--locked"], &env);
    assert!(out.contains("as the identical commit"), "{out}");
}

#[test]
fn an_incremental_build_reproduces_the_clean_build() {
    let fx = fixture();
    record_provenance(&fx.root);
    topic(&fx, "t1", "c.txt", "from t1\n");
    topic(&fx, "t2", "d.txt", "from t2\n");
    add_branch(&fx, "t1");
    add_branch(&fx, "t2");
    ff_ok(&fx.root, &["build"]);
    let prefix = lock_json(&fx.root)["build"]["pre_provenance_commit"]
        .as_str()
        .unwrap()
        .to_string();

    // Committed far in the future, so a scheme that dated the whole build from
    // its newest input would move every earlier commit.
    git(&fx.upstream, &["checkout", "-q", "-b", "t3", "main"]);
    git(&fx.upstream, &["checkout", "-q", "main"]);
    commit_dated(&fx, "t3", "e.txt", "from t3\n", 4_000_000_000);
    add_branch(&fx, "t3");
    let out = ff_ok(&fx.root, &["build"]);
    assert!(out.contains("extending the locked build"), "{out}");
    let commit = locked_commit(&fx.root);
    assert!(git_try(
        &build_worktree(&fx),
        &["merge-base", "--is-ancestor", &prefix, &commit]
    ));

    fs::remove_dir_all(build_worktree(&fx)).unwrap();
    let out = ff_ok(&fx.root, &["build", "--locked"]);
    assert!(out.contains("building from base"), "{out}");
    assert!(
        out.contains(&format!("as the identical commit {}", &commit[..12])),
        "{out}"
    );
}

/// Every generated commit carries the fixed identity and is dated one second
/// after its newest parent, in UTC.
#[test]
fn generated_commits_carry_a_fixed_identity_and_parent_derived_dates() {
    let fx = fixture();
    record_provenance(&fx.root);
    topic(&fx, "t1", "c.txt", "from t1\n");
    add_branch(&fx, "t1");
    ff_ok(&fx.root, &["build"]);
    let wt = build_worktree(&fx);

    let log = git(&wt, &["log", "--format=%H %ct %P"]);
    let dates: BTreeMap<&str, i64> = log
        .lines()
        .map(|line| {
            let mut fields = line.split(' ');
            (
                fields.next().unwrap(),
                fields.next().unwrap().parse().unwrap(),
            )
        })
        .collect();
    let generated = git(&wt, &["log", "--committer=fork-assembler", "--format=%H"]);
    assert_eq!(
        generated.lines().count(),
        2,
        "a merge and the provenance commit"
    );
    for oid in generated.lines() {
        let meta = git(
            &wt,
            &[
                "log",
                "-1",
                "--format=%an <%ae>|%cn <%ce>|%ad|%P",
                "--date=raw",
                oid,
            ],
        );
        let fields: Vec<&str> = meta.split('|').collect();
        let ident = "fork-assembler <fork-assembler@noreply.invalid>";
        assert_eq!(fields[0], ident, "{meta}");
        assert_eq!(fields[1], ident, "{meta}");
        let newest = fields[3].split(' ').map(|p| dates[p]).max().unwrap();
        assert_eq!(fields[2], format!("{} +0000", newest + 1), "{meta}");
        assert_eq!(dates[oid], newest + 1, "{meta}");
    }
}

/// A lock written before commits were deterministic records a commit no build
/// can reproduce, so nothing may extend it or call it up to date, and a locked
/// reproduction can only check its tree.
#[test]
fn a_build_from_before_deterministic_commits_is_rebuilt() {
    let fx = fixture();
    topic(&fx, "t1", "c.txt", "from t1\n");
    add_branch(&fx, "t1");
    ff_ok(&fx.root, &["build"]);
    let path = fx.root.join("manifest.lock.json");
    let mut lock = lock_json(&fx.root);
    lock["build"].as_object_mut().unwrap().remove("protocol");
    fs::write(&path, serde_json::to_string_pretty(&lock).unwrap()).unwrap();

    let out = ff_ok(&fx.root, &["status"]);
    assert!(out.contains("commit protocol 0"), "{out}");
    let out = ff_ok(&fx.root, &["build", "--locked"]);
    assert!(out.contains("predates deterministic commits"), "{out}");

    let out = ff_ok(&fx.root, &["build"]);
    assert!(!out.contains("up to date"), "{out}");
    assert!(out.contains("building from base"), "{out}");
    assert_eq!(lock_json(&fx.root)["build"]["protocol"], 1);
}

/// The conflict style belongs to the recipe, not the machine: tracked pairs
/// replay only under the style they were recorded with, so the operator's own
/// `merge.conflictStyle` must neither shape the conflict they resolve nor stop
/// a later replay.
#[test]
fn the_manifest_chooses_the_conflict_style() {
    for (style, shows_base) in [(None, true), (Some("merge"), false)] {
        let fx = fixture();
        if let Some(style) = style {
            let path = fx.root.join("manifest.toml");
            let manifest = fs::read_to_string(&path).unwrap();
            fs::write(&path, format!("conflict_style = \"{style}\"\n{manifest}")).unwrap();
        }
        topic(&fx, "t1", "b.txt", "one\nT1\nthree\n");
        topic(&fx, "t2", "b.txt", "one\nT2\nthree\n");
        add_branch(&fx, "t1");
        add_branch(&fx, "t2");

        // Each machine's own preference is the opposite of the manifest's.
        let mine = fx._tmp.path().join("mine.gitconfig");
        let theirs = if shows_base { "merge" } else { "diff3" };
        fs::write(&mine, format!("[merge]\n\tconflictStyle = {theirs}\n")).unwrap();
        let env = [("GIT_CONFIG_GLOBAL", mine.to_str().unwrap())];
        let out = ff_env(&fx.root, &["build"], &env);
        assert_eq!(out.status.code(), Some(2));
        let wt = build_worktree(&fx);
        assert_eq!(read(wt.join("b.txt")).contains("|||||||"), shows_base);

        fs::write(wt.join("b.txt"), "one\nT1+T2\nthree\n").unwrap();
        git(&wt, &["add", "b.txt"]);
        assert!(ff_env(&fx.root, &["continue"], &env).status.success());
        let commit = locked_commit(&fx.root);

        fs::remove_dir_all(&wt).unwrap();
        let other = fx._tmp.path().join("other.gitconfig");
        fs::write(&other, "[merge]\n\tconflictStyle = zdiff3\n").unwrap();
        let out = ff_env(
            &fx.root,
            &["build", "--locked"],
            &[("GIT_CONFIG_GLOBAL", other.to_str().unwrap())],
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{stdout}");
        assert!(stdout.contains("auto-resolved"), "{stdout}");
        assert!(
            stdout.contains(&format!("identical commit {}", &commit[..12])),
            "{stdout}"
        );
    }
}
