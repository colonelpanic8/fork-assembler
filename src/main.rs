use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

const TPL_FLAKE: &str = include_str!("../templates/maintenance/flake.nix");
const TPL_ENVRC: &str = include_str!("../templates/maintenance/.envrc");
const TPL_GITIGNORE: &str = include_str!("../templates/maintenance/.gitignore");
const TPL_JUSTFILE: &str = include_str!("../templates/maintenance/justfile");
const TPL_README: &str = include_str!("../templates/maintenance/README.md");
const TPL_MANIFEST: &str = include_str!("../templates/maintenance/manifest.toml");
const TPL_AGENTS: &str = include_str!("../templates/maintenance/AGENTS.md");
const TPL_CLAUDE: &str = include_str!("../templates/maintenance/CLAUDE.md");
const TPL_SKILL: &str = include_str!("../templates/maintenance/.claude/skills/fork-fold/SKILL.md");

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
    /// Fetch tracked refs and assemble the stack, applying tracked resolutions
    Build {
        /// Build from the lock's pinned OIDs without fetching
        #[arg(long)]
        locked: bool,
    },
    /// Resume a build stopped on a conflict; record or rewrite its resolution
    Continue,
    /// Append an entry to the manifest
    Add {
        /// Branch entry as remote:branch
        target: Option<String>,
        /// PR number (refs/pull/N/head on the base remote)
        #[arg(long)]
        pr: Option<u64>,
        /// Tracked patch file applied on top
        #[arg(long)]
        patch: Option<String>,
    },
    /// Remove an entry from the manifest
    Remove { name: String },
    /// Compare lock vs. manifest vs. live refs
    Status,
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
        (".claude/skills/fork-fold/SKILL.md", TPL_SKILL),
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
    for sub in ["resolutions", "patches"] {
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
        Command::Build { .. } => bail!("not yet implemented"),
        Command::Continue => bail!("not yet implemented"),
        Command::Add { .. } => bail!("not yet implemented"),
        Command::Remove { .. } => bail!("not yet implemented"),
        Command::Status => bail!("not yet implemented"),
    }
}
