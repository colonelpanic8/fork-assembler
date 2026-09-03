//! `init`: scaffold a maintenance repository from the bundled template.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

const TPL_FLAKE: &str = include_str!("../templates/maintenance/flake.nix");
const TPL_ENVRC: &str = include_str!("../templates/maintenance/.envrc");
const TPL_GITIGNORE: &str = include_str!("../templates/maintenance/.gitignore");
const TPL_JUSTFILE: &str = include_str!("../templates/maintenance/justfile");
const TPL_README: &str = include_str!("../templates/maintenance/README.md");
const TPL_MANIFEST: &str = include_str!("../templates/maintenance/manifest.toml");
const TPL_AGENTS: &str = include_str!("../templates/maintenance/AGENTS.md");
const TPL_CLAUDE: &str = include_str!("../templates/maintenance/CLAUDE.md");
const TPL_SKILL: &str =
    include_str!("../templates/maintenance/.agents/skills/fork-assembler/SKILL.md");

/// Files copied verbatim, never overwritten.
const STATIC_FILES: [(&str, &str); 8] = [
    ("flake.nix", TPL_FLAKE),
    (".envrc", TPL_ENVRC),
    (".gitignore", TPL_GITIGNORE),
    ("justfile", TPL_JUSTFILE),
    ("README.md", TPL_README),
    ("AGENTS.md", TPL_AGENTS),
    ("CLAUDE.md", TPL_CLAUDE),
    (".agents/skills/fork-assembler/SKILL.md", TPL_SKILL),
];

/// git with inherited stdio, so the operator sees what a clone or submodule
/// add is doing.
fn run_git(dir: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .context("failed to run git")?;
    if !status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(())
}

pub fn init(
    dir: PathBuf,
    upstream: Option<String>,
    base_ref: String,
    submodule: bool,
) -> Result<()> {
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

    for (name, content) in STATIC_FILES {
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
        let link = link_dir.join("fork-assembler");
        if fs::symlink_metadata(&link).is_err() {
            std::os::unix::fs::symlink("../../.agents/skills/fork-assembler", &link)?;
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
        "initialized fork-assembler maintenance repo in {}",
        dir.display()
    );
    println!("next: edit manifest.toml, `direnv allow`, then `fork-assembler add` / `fork-assembler build`");
    Ok(())
}
