//! Fixtures shared by the integration tests: drive the compiled binary
//! against synthetic git repos. No network — every remote is a local path
//! repository.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_fork-assembler")
}

pub fn git(dir: &Path, args: &[&str]) -> String {
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

pub fn ff(dir: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("fork-assembler runs")
}

pub fn ff_ok(dir: &Path, args: &[&str]) -> String {
    let out = ff(dir, args);
    assert!(
        out.status.success(),
        "fork-assembler {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

pub fn ff_stopped(dir: &Path, args: &[&str]) -> String {
    let out = ff(dir, args);
    assert_eq!(
        out.status.code(),
        Some(2),
        "fork-assembler {args:?} expected exit 2:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

pub struct Fixture {
    pub _tmp: TempDir,
    pub upstream: PathBuf,
    pub root: PathBuf,
}

/// An upstream repo with a base commit on main, plus a maintenance root whose
/// manifest points at it. Topics are branches created in the upstream repo.
pub fn fixture() -> Fixture {
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
pub fn topic(fx: &Fixture, name: &str, file: &str, content: &str) {
    git(&fx.upstream, &["checkout", "-q", "-b", name, "main"]);
    fs::write(fx.upstream.join(file), content).unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", name]);
    git(&fx.upstream, &["checkout", "-q", "main"]);
}

pub fn add_branch(fx: &Fixture, name: &str) {
    ff_ok(&fx.root, &["add", &format!("up:{name}")]);
}

/// git that is allowed to fail, for hand-making the conflicted merges a real
/// combined branch contains.
pub fn git_try(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
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
        .expect("git runs")
        .status
        .success()
}

/// A combined branch as it exists in the wild: it merges each parent in turn
/// and then adds work of its own on top. Passing an empty `file` makes it a
/// pure merge of its parents.
pub fn combined(fx: &Fixture, name: &str, parents: &[&str], file: &str, content: &str) {
    git(&fx.upstream, &["checkout", "-q", "-b", name, "main"]);
    for parent in parents {
        git(
            &fx.upstream,
            &[
                "merge",
                "-q",
                "--no-ff",
                "-m",
                &format!("merge {parent}"),
                parent,
            ],
        );
    }
    if !file.is_empty() {
        fs::write(fx.upstream.join(file), content).unwrap();
        git(&fx.upstream, &["add", "-A"]);
        git(
            &fx.upstream,
            &["commit", "-q", "-m", &format!("{name} own work")],
        );
    }
    git(&fx.upstream, &["checkout", "-q", "main"]);
}

/// Carry `name` as a derived entry whose parents are the named branches.
pub fn add_derived(fx: &Fixture, name: &str, parents: &[&str]) {
    let mut args = vec!["add".to_string(), format!("up:{name}")];
    for parent in parents {
        args.push("--parent".into());
        args.push(format!("up:{parent}"));
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    ff_ok(&fx.root, &args);
}

pub fn publish_derived_to_its_branch(fx: &Fixture, name: &str) {
    let path = fx.root.join("manifest.toml");
    let manifest = fs::read_to_string(&path).unwrap();
    let branch = format!("branch = \"up:{name}\"");
    let replacement = format!("{branch}\nreconstruction_publish = \"up:{name}\"");
    assert!(
        manifest.contains(&branch),
        "missing derived branch {branch:?}"
    );
    fs::write(path, manifest.replacen(&branch, &replacement, 1)).unwrap();
}

pub fn build_worktree(fx: &Fixture) -> PathBuf {
    fx.root.join(".worktrees/build")
}

pub fn derive_worktree(fx: &Fixture) -> PathBuf {
    fx.root.join(".worktrees/derive")
}

pub fn read(path: PathBuf) -> String {
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

pub fn append_manifest(root: &Path, text: &str) {
    let path = root.join("manifest.toml");
    let current = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("{current}{text}")).unwrap();
}

/// The stderr of a command that must fail.
pub fn ff_err(dir: &Path, args: &[&str]) -> String {
    let out = ff(dir, args);
    assert!(
        !out.status.success(),
        "fork-assembler {args:?} unexpectedly succeeded:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stderr).to_string()
}

pub fn lock_json(root: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(root.join("manifest.lock.json")).unwrap()).unwrap()
}

/// Hash directories under resolutions/rerere/ (tracked pair entries).
pub fn pair_dirs(root: &Path) -> Vec<PathBuf> {
    let dir = root.join("resolutions/rerere");
    if !dir.exists() {
        return Vec::new();
    }
    let mut dirs: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().unwrap().is_dir())
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

/// A plain unified diff rewriting `file` to `content`, taken against `branch`
/// so it applies at that branch's position in the stack.
pub fn patch_against(fx: &Fixture, branch: &str, file: &str, content: &str) -> String {
    git(
        &fx.upstream,
        &["checkout", "-q", "-b", "scratch-patch", branch],
    );
    fs::write(fx.upstream.join(file), content).unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "scratch"]);
    let diff = git(&fx.upstream, &["diff", branch, "HEAD"]);
    git(&fx.upstream, &["checkout", "-q", "main"]);
    git(&fx.upstream, &["branch", "-q", "-D", "scratch-patch"]);
    diff + "\n"
}

pub fn write_patch(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// Subjects of the assembled branch, oldest first.
pub fn subjects(worktree: &Path) -> Vec<String> {
    git(worktree, &["log", "--format=%s"])
        .lines()
        .rev()
        .map(str::to_string)
        .collect()
}

/// The commit whose subject is exactly `subject`.
pub fn commit_by_subject(worktree: &Path, subject: &str) -> String {
    git(worktree, &["log", "--format=%H%x1f%s"])
        .lines()
        .find_map(|line| line.split_once('\x1f').filter(|(_, s)| *s == subject))
        .unwrap_or_else(|| panic!("no commit titled {subject:?}"))
        .0
        .to_string()
}

/// Move upstream main on, so a topic branched before it goes stale.
pub fn advance_main(fx: &Fixture, file: &str, content: &str) {
    fs::write(fx.upstream.join(file), content).unwrap();
    git(&fx.upstream, &["add", "-A"]);
    git(&fx.upstream, &["commit", "-q", "-m", "upstream moves on"]);
}
