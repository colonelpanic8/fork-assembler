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
    /// Resume a build stopped on a conflict; harvest its rerere pairs
    Continue,
    /// Append entries to the manifest (idempotent)
    Add {
        /// Branch entry as remote:branch
        target: Option<String>,
        /// PR number (refs/pull/N/head on the base remote)
        #[arg(long)]
        pr: Option<u64>,
        /// Tracked patch file applied on top
        #[arg(long)]
        patch: Option<String>,
        /// Append every open PR authored by this user on the base repo that
        /// is not already carried
        #[arg(long, value_name = "USER")]
        prs_from: Option<String>,
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
    let mut appended = 0usize;

    if let Some(branch) = target {
        if !branch.contains(':') {
            bail!("branch entries are REMOTE:BRANCH (remote names come from [remotes])");
        }
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
        for (n, title) in prs {
            if existing.prs.contains(&n) {
                println!("already carried: pr {n} ({title})");
                continue;
            }
            push_entry(&mut doc, |t| {
                t["pr"] = value(n);
                t["summary"] = value(&title);
            });
            println!("added pr {n} ({title})");
            appended += 1;
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
        Command::Build { locked } => {
            let code = engine::build(&std::env::current_dir()?, locked)?;
            std::process::exit(code);
        }
        Command::Update { entries } => ops::update(&std::env::current_dir()?, &entries),
        Command::Continue => {
            let code = engine::cont(&std::env::current_dir()?)?;
            std::process::exit(code);
        }
        Command::Remove { name } => {
            let root = std::env::current_dir()?;
            let earliest = manifest::remove_entries(&root, &[name])?;
            println!(
                "entry removed; the build suffix from position {} is invalidated -- run `fork-fold build`",
                earliest + 1
            );
            Ok(())
        }
        Command::Prune { dry_run } => ops::prune(&std::env::current_dir()?, dry_run),
        Command::Status { live } => ops::status(&std::env::current_dir()?, live),
    }
}
