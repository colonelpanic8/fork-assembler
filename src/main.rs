mod engine;
mod git;
mod lock;
mod manifest;
mod ops;
mod rerere;
mod source;
mod state;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

const TPL_FLAKE: &str = include_str!("../templates/maintenance/flake.nix");
const TPL_ENVRC: &str = include_str!("../templates/maintenance/.envrc");
const TPL_GITIGNORE: &str = include_str!("../templates/maintenance/.gitignore");
const TPL_JUSTFILE: &str = include_str!("../templates/maintenance/justfile");
const TPL_README: &str = include_str!("../templates/maintenance/README.md");
const TPL_MANIFEST: &str = include_str!("../templates/maintenance/manifest.toml");
const TPL_AGENTS: &str = include_str!("../templates/maintenance/AGENTS.md");
const TPL_CLAUDE: &str = include_str!("../templates/maintenance/CLAUDE.md");
const TPL_SKILL: &str = include_str!("../templates/maintenance/.agents/skills/fork-fold/SKILL.md");

#[derive(Parser)]
#[command(name = "fork-fold", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scaffold a maintenance repository (manifest, resolutions, dev shell, direnv)
    Init {
        /// Directory to initialize (default: current directory)
        dir: Option<PathBuf>,
        /// Upstream repository URL for the base
        #[arg(long)]
        upstream: Option<String>,
        /// Base ref on the upstream remote
        #[arg(long, default_value = "main")]
        base_ref: String,
        /// Add the upstream as a git submodule sourcing the base objects
        #[arg(long, requires = "upstream")]
        submodule: bool,
    },
    /// Assemble the stack from the lock's pins, applying tracked resolutions
    Build {
        /// Refuse network access and refuse to pin new entries
        #[arg(long)]
        locked: bool,
    },
    /// Repin the base and entries to live remote heads (the batch bump)
    Update {
        /// Entries to repin (default: base and all entries)
        entries: Vec<String>,
    },
    /// Resume a build stopped on a conflict or a fixup; harvest its rerere pairs
    Continue,
    /// Append entries to the manifest (idempotent)
    Add {
        /// Branch entry as remote:branch
        target: Option<String>,
        /// PR number (refs/pull/N/head on the base remote)
        #[arg(long)]
        pr: Option<u64>,
        /// Standalone patch entry at its own position (for a cross-entry
        /// repair, attach a fixup to the responsible entry instead)
        #[arg(long)]
        patch: Option<String>,
        /// Append every open PR authored by this user on the base repo that
        /// is not already carried
        #[arg(long, value_name = "USER")]
        prs_from: Option<String>,
    },
    /// Record a target as deliberately not carried, so discovery cannot
    /// re-admit it (does not touch the lock; nothing needs rebuilding)
    Exclude {
        /// Branch target as remote:branch
        target: Option<String>,
        /// PR number to refuse
        #[arg(long)]
        pr: Option<u64>,
        /// Patch file to refuse
        #[arg(long)]
        patch: Option<String>,
        /// Why this target stays out; quoted wherever the refusal is reported
        #[arg(long)]
        reason: Option<String>,
    },
    /// Attach a coherence fixup to an entry: a patch applied as part of that
    /// entry's own merge step, so the entry boundary is never an invalid tree
    Fixup {
        /// Entry the fixup belongs to (the one whose admission broke coherence)
        entry: String,
        /// Patch file, relative to the repository root
        path: Option<String>,
        /// Write PATH from the build worktree first: its uncommitted changes,
        /// or the entry's existing fixup commit when the worktree is clean
        #[arg(long, requires = "path")]
        capture: bool,
        /// Detach the entry's fixup (the patch file is left in place)
        #[arg(long, conflicts_with_all = ["path", "capture"])]
        remove: bool,
    },
    /// Remove an entry from the manifest
    Remove { name: String },
    /// Drop entries whose changes have landed in the base
    Prune {
        /// Report what would be pruned without changing the manifest
        #[arg(long)]
        dry_run: bool,
    },
    /// Compare lock vs. manifest (offline; --live also checks remote heads)
    Status {
        /// Fetch live heads and report pins that are behind
        #[arg(long)]
        live: bool,
    },
}

fn run_git(dir: &Path, args: &[&str]) -> Result<()> {
    let status = Process::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .context("failed to run git")?;
    if !status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(())
}

fn init(dir: PathBuf, upstream: Option<String>, base_ref: String, submodule: bool) -> Result<()> {
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let manifest_path = dir.join("manifest.toml");
    if manifest_path.exists() {
        bail!("{} already exists", manifest_path.display());
    }

    let mut manifest = TPL_MANIFEST.replace("ref = \"main\"", &format!("ref = {base_ref:?}"));
    if let Some(url) = &upstream {
        manifest = manifest.replace(
            "# upstream = \"https://github.com/OWNER/REPO\"",
            &format!("upstream = {url:?}"),
        );
    }
    if submodule {
        manifest = manifest.replace(
            "# submodule = \"upstream\"  # optional",
            "submodule = \"upstream\"  # optional",
        );
    }

    let static_files = [
        ("flake.nix", TPL_FLAKE),
        (".envrc", TPL_ENVRC),
        (".gitignore", TPL_GITIGNORE),
        ("justfile", TPL_JUSTFILE),
        ("README.md", TPL_README),
        ("AGENTS.md", TPL_AGENTS),
        ("CLAUDE.md", TPL_CLAUDE),
        (".agents/skills/fork-fold/SKILL.md", TPL_SKILL),
    ];
    for (name, content) in static_files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !path.exists() {
            fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
        }
    }
    fs::write(&manifest_path, manifest)?;
    // Per-agent skill discovery paths point at the canonical .agents/skills.
    #[cfg(unix)]
    for agent_dir in [".claude/skills", ".codex/skills"] {
        let link_dir = dir.join(agent_dir);
        fs::create_dir_all(&link_dir)?;
        let link = link_dir.join("fork-fold");
        if !link.exists() && fs::symlink_metadata(&link).is_err() {
            std::os::unix::fs::symlink("../../.agents/skills/fork-fold", &link)?;
        }
    }
    for sub in ["resolutions/rerere", "patches"] {
        let path = dir.join(sub);
        fs::create_dir_all(&path)?;
        fs::write(path.join(".gitkeep"), "")?;
    }

    if !dir.join(".git").exists() {
        run_git(&dir, &["init", "-b", "main"])?;
    }
    if submodule {
        let url = upstream.expect("clap enforces --upstream with --submodule");
        run_git(&dir, &["submodule", "add", &url, "upstream"])?;
    }

    println!(
        "initialized fork-fold maintenance repo in {}",
        dir.display()
    );
    println!("next: edit manifest.toml, `direnv allow`, then `fork-fold add` / `fork-fold build`");
    Ok(())
}

fn load_manifest() -> Result<(PathBuf, DocumentMut)> {
    let path = PathBuf::from("manifest.toml");
    if !path.exists() {
        bail!("no manifest.toml in the current directory (run from the maintenance repo root)");
    }
    let doc = fs::read_to_string(&path)?
        .parse::<DocumentMut>()
        .context("parsing manifest.toml")?;
    Ok((path, doc))
}

struct ExistingEntries {
    branches: HashSet<String>,
    prs: HashSet<i64>,
    patches: HashSet<String>,
}

fn existing_entries(doc: &DocumentMut) -> ExistingEntries {
    let mut e = ExistingEntries {
        branches: HashSet::new(),
        prs: HashSet::new(),
        patches: HashSet::new(),
    };
    if let Some(entries) = doc.get("entry").and_then(Item::as_array_of_tables) {
        for t in entries {
            if let Some(b) = t.get("branch").and_then(Item::as_str) {
                e.branches.insert(b.to_string());
            }
            if let Some(n) = t.get("pr").and_then(Item::as_integer) {
                e.prs.insert(n);
            }
            if let Some(p) = t.get("patch").and_then(Item::as_str) {
                e.patches.insert(p.to_string());
            }
        }
    }
    e
}

/// The exclusions recorded in the live document, in manifest order.
fn existing_exclusions(doc: &DocumentMut) -> Result<Vec<manifest::Exclusion>> {
    let Some(tables) = doc.get("exclude").and_then(Item::as_array_of_tables) else {
        return Ok(Vec::new());
    };
    tables
        .iter()
        .map(|t| {
            manifest::Exclusion::from_fields(
                t.get("branch").and_then(Item::as_str).map(str::to_string),
                t.get("pr").and_then(Item::as_integer),
                t.get("patch").and_then(Item::as_str).map(str::to_string),
                t.get("reason").and_then(Item::as_str).map(str::to_string),
            )
        })
        .collect()
}

/// Refuse an explicitly requested target that the manifest excludes.
///
/// Discovery sweeps are bulk and impersonal, so `--prs-from` skips excluded
/// PRs with a note. Naming one on the command line is a decision, and the
/// maintainer deserves to learn it contradicts a recorded one rather than
/// watch the request vanish.
fn reject_if_excluded(exclusions: &[manifest::Exclusion], target: &manifest::Target) -> Result<()> {
    if let Some(exclusion) = exclusions.iter().find(|x| &x.target == target) {
        bail!(
            "{} is excluded by the manifest: {}\n\
             delete that [[exclude]] first if the refusal no longer holds",
            target.label(),
            exclusion
                .reason
                .clone()
                .unwrap_or_else(|| "no reason recorded".into())
        );
    }
    Ok(())
}

fn push_entry(doc: &mut DocumentMut, fill: impl FnOnce(&mut Table)) {
    let entries = doc
        .entry("entry")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
    let arr = entries
        .as_array_of_tables_mut()
        .expect("entry is an array of tables");
    let mut t = Table::new();
    fill(&mut t);
    arr.push(t);
}

/// owner/repo slug of the base remote, for gh.
fn base_repo_slug(doc: &DocumentMut) -> Result<String> {
    let base_remote = doc
        .get("base")
        .and_then(|b| b.get("remote"))
        .and_then(Item::as_str)
        .unwrap_or("upstream");
    let url = doc
        .get("remotes")
        .and_then(|r| r.get(base_remote))
        .and_then(Item::as_str)
        .with_context(|| format!("remote {base_remote:?} not defined under [remotes]"))?;
    manifest::slug_from_url(url)
}

fn open_prs_by(slug: &str, author: &str) -> Result<Vec<(i64, String)>> {
    let out = Process::new("gh")
        .args([
            "pr",
            "list",
            "-R",
            slug,
            "--author",
            author,
            "--state",
            "open",
            "--json",
            "number,title",
            "--limit",
            "500",
        ])
        .output()
        .context("failed to run gh")?;
    if !out.status.success() {
        bail!(
            "gh pr list failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let mut prs = Vec::new();
    for item in parsed.as_array().context("unexpected gh output")? {
        let number = item["number"].as_i64().context("pr without number")?;
        let title = item["title"].as_str().unwrap_or("").to_string();
        prs.push((number, title));
    }
    prs.sort();
    Ok(prs)
}

/// What a discovery sweep decides about one PR it found.
#[derive(Debug, PartialEq, Eq)]
enum Discovered {
    Append,
    AlreadyCarried,
    Excluded(String),
}

/// Sort discovered PRs into append / already-carried / excluded.
///
/// Split out from the sweep so the decision is testable without `gh`: this is
/// the whole of what `--prs-from` decides, and the exclusion rule is the part
/// worth proving.
fn triage_discovered(
    prs: &[(i64, String)],
    carried: &HashSet<i64>,
    exclusions: &[manifest::Exclusion],
) -> Vec<(i64, String, Discovered)> {
    prs.iter()
        .map(|(number, title)| {
            let verdict = if carried.contains(number) {
                Discovered::AlreadyCarried
            } else if let Some(exclusion) = exclusions
                .iter()
                .find(|x| x.target == manifest::Target::Pr { number: *number })
            {
                Discovered::Excluded(
                    exclusion
                        .reason
                        .clone()
                        .unwrap_or_else(|| "no reason recorded".into()),
                )
            } else {
                Discovered::Append
            };
            (*number, title.clone(), verdict)
        })
        .collect()
}

fn exclude(
    target: Option<String>,
    pr: Option<u64>,
    patch: Option<String>,
    reason: Option<String>,
) -> Result<()> {
    let target = manifest::Exclusion::from_fields(target, pr.map(|n| n as i64), patch, None)
        .context("nothing to exclude: pass REMOTE:BRANCH, --pr, or --patch")?
        .target;
    let root = PathBuf::from(".");
    match manifest::record_exclusion(&root, &target, reason.as_deref())? {
        manifest::Excluded::Added => {
            println!("excluded {}", target.label());
            if reason.is_none() {
                println!(
                    "  no reason recorded -- add one with `--reason` so the refusal \
                     is still legible later"
                );
            }
            println!("  the lock is untouched; nothing needs rebuilding");
        }
        manifest::Excluded::AlreadyRecorded => {
            println!("already excluded: {}", target.label())
        }
        manifest::Excluded::ReasonUpdated { previous } => {
            println!("already excluded: {}", target.label());
            match previous {
                Some(previous) => println!("  reason updated (was: {previous})"),
                None => println!("  reason recorded"),
            }
        }
    }
    Ok(())
}

fn add(
    target: Option<String>,
    pr: Option<u64>,
    patch: Option<String>,
    prs_from: Option<String>,
) -> Result<()> {
    if target.is_none() && pr.is_none() && patch.is_none() && prs_from.is_none() {
        bail!("nothing to add: pass REMOTE:BRANCH, --pr, --patch, or --prs-from");
    }
    let (path, mut doc) = load_manifest()?;
    let existing = existing_entries(&doc);
    let exclusions = existing_exclusions(&doc)?;
    let mut appended = 0usize;

    if let Some(branch) = target {
        if !branch.contains(':') {
            bail!("branch entries are REMOTE:BRANCH (remote names come from [remotes])");
        }
        reject_if_excluded(
            &exclusions,
            &manifest::Target::Branch {
                spec: branch.clone(),
            },
        )?;
        if existing.branches.contains(&branch) {
            println!("already carried: {branch}");
        } else {
            push_entry(&mut doc, |t| {
                t["branch"] = value(&branch);
            });
            println!("added branch {branch}");
            appended += 1;
        }
    }
    if let Some(n) = pr {
        let n = n as i64;
        reject_if_excluded(&exclusions, &manifest::Target::Pr { number: n })?;
        if existing.prs.contains(&n) {
            println!("already carried: pr {n}");
        } else {
            push_entry(&mut doc, |t| {
                t["pr"] = value(n);
            });
            println!("added pr {n}");
            appended += 1;
        }
    }
    if let Some(file) = patch {
        reject_if_excluded(&exclusions, &manifest::Target::Patch { path: file.clone() })?;
        if existing.patches.contains(&file) {
            println!("already carried: patch {file}");
        } else {
            push_entry(&mut doc, |t| {
                t["patch"] = value(&file);
            });
            println!("added patch {file}");
            appended += 1;
        }
    }
    if let Some(author) = prs_from {
        let slug = base_repo_slug(&doc)?;
        let prs = open_prs_by(&slug, &author)?;
        if prs.is_empty() {
            println!("no open PRs by {author} on {slug}");
        }
        for (n, title, verdict) in triage_discovered(&prs, &existing.prs, &exclusions) {
            match verdict {
                Discovered::AlreadyCarried => println!("already carried: pr {n} ({title})"),
                Discovered::Excluded(reason) => {
                    println!("excluded: pr {n} ({title}) -- not added: {reason}")
                }
                Discovered::Append => {
                    push_entry(&mut doc, |t| {
                        t["pr"] = value(n);
                        t["summary"] = value(&title);
                    });
                    println!("added pr {n} ({title})");
                    appended += 1;
                }
            }
        }
    }

    if appended > 0 {
        fs::write(&path, doc.to_string())?;
        println!(
            "{appended} entr{} appended to {}",
            if appended == 1 { "y" } else { "ies" },
            path.display()
        );
    } else {
        println!("manifest unchanged");
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init {
            dir,
            upstream,
            base_ref,
            submodule,
        } => init(
            dir.unwrap_or_else(|| PathBuf::from(".")),
            upstream,
            base_ref,
            submodule,
        ),
        Command::Add {
            target,
            pr,
            patch,
            prs_from,
        } => add(target, pr, patch, prs_from),
        Command::Exclude {
            target,
            pr,
            patch,
            reason,
        } => exclude(target, pr, patch, reason),
        Command::Build { locked } => {
            let code = engine::build(&std::env::current_dir()?, locked)?;
            std::process::exit(code);
        }
        Command::Update { entries } => ops::update(&std::env::current_dir()?, &entries),
        Command::Continue => {
            let code = engine::cont(&std::env::current_dir()?)?;
            std::process::exit(code);
        }
        Command::Fixup {
            entry,
            path,
            capture,
            remove,
        } => ops::fixup(
            &std::env::current_dir()?,
            &entry,
            path.as_deref(),
            capture,
            remove,
        ),
        Command::Remove { name } => {
            let root = std::env::current_dir()?;
            let removal = manifest::remove_entries(&root, &[name])?;
            println!(
                "entry removed; the build suffix from position {} is invalidated -- run `fork-fold build`",
                removal.earliest + 1
            );
            ops::report_orphaned_fixups(&removal);
            Ok(())
        }
        Command::Prune { dry_run } => ops::prune(&std::env::current_dir()?, dry_run),
        Command::Status { live } => ops::status(&std::env::current_dir()?, live),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found() -> Vec<(i64, String)> {
        vec![
            (7, "superseded".to_string()),
            (8, "carried".to_string()),
            (9, "fresh".to_string()),
        ]
    }

    fn excluding(number: i64, reason: Option<&str>) -> manifest::Exclusion {
        manifest::Exclusion {
            target: manifest::Target::Pr { number },
            reason: reason.map(str::to_string),
        }
    }

    /// The sweep's whole job: an excluded PR is skipped with its reason while
    /// everything else it found is still appended.
    #[test]
    fn discovery_skips_excluded_prs_and_appends_the_rest() {
        let carried = HashSet::from([8]);
        let excludes = vec![excluding(7, Some("superseded by 10"))];
        let verdicts = triage_discovered(&found(), &carried, &excludes);
        assert_eq!(
            verdicts.iter().map(|(n, _, v)| (*n, v)).collect::<Vec<_>>(),
            vec![
                (7, &Discovered::Excluded("superseded by 10".into())),
                (8, &Discovered::AlreadyCarried),
                (9, &Discovered::Append),
            ]
        );
    }

    /// An exclusion without a reason still bites; only the message changes.
    #[test]
    fn discovery_skips_an_exclusion_with_no_reason() {
        let verdicts = triage_discovered(&found(), &HashSet::new(), &[excluding(7, None)]);
        assert_eq!(
            verdicts[0].2,
            Discovered::Excluded("no reason recorded".into())
        );
    }

    /// Carried wins the report when a target is somehow both: the manifest
    /// load rejects that state, so the sweep never needs to arbitrate it.
    #[test]
    fn discovery_reports_carried_before_excluded() {
        let verdicts = triage_discovered(
            &found(),
            &HashSet::from([7]),
            &[excluding(7, Some("stale"))],
        );
        assert_eq!(verdicts[0].2, Discovered::AlreadyCarried);
    }
}
