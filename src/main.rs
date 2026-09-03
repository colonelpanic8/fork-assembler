mod add;
mod engine;
mod git;
mod init;
mod lock;
mod manifest;
mod ops;
mod report;
mod rerere;
mod source;
mod state;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "fork-assembler", version, about)]
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
        /// A PR number or REMOTE:BRANCH this entry merged in and builds on.
        /// Repeatable, in the order the entry merged them. Declaring parents
        /// makes the entry derived: `build` reconstructs it from them instead
        /// of merging its pin, and discovery stops offering them
        #[arg(long = "parent", value_name = "P")]
        parents: Vec<String>,
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = std::env::current_dir()?;
    match cli.command {
        Command::Init {
            dir,
            upstream,
            base_ref,
            submodule,
        } => init::init(dir.unwrap_or(root), upstream, base_ref, submodule),
        Command::Add {
            target,
            pr,
            patch,
            parents,
            prs_from,
        } => add::add(&root, target, pr, patch, parents, prs_from),
        Command::Exclude {
            target,
            pr,
            patch,
            reason,
        } => add::exclude(&root, target, pr, patch, reason),
        Command::Build { locked } => std::process::exit(engine::build(&root, locked)?),
        Command::Continue => std::process::exit(engine::cont(&root)?),
        Command::Update { entries } => ops::update(&root, &entries),
        Command::Fixup {
            entry,
            path,
            capture,
            remove,
        } => ops::fixup(&root, &entry, path.as_deref(), capture, remove),
        Command::Remove { name } => ops::remove(&root, name),
        Command::Prune { dry_run } => ops::prune(&root, dry_run),
        Command::Status { live } => ops::status(&root, live),
    }
}
