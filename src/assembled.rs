//! `assembled`: the assembled commit a maintenance commit built, on demand.
//!
//! Nothing keeps old assembled commits alive, and nothing has to: each
//! maintenance commit's recipe regenerates exactly the commit its lock names.
//! This makes that commit present — found, or regenerated — prints it, and can
//! check it out, so building or inspecting any historical stack is one command.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::Serialize;

use crate::git;
use crate::manifest;
use crate::recipe::{Recipe, Source};
use crate::report::Report;
use crate::source;

#[derive(Serialize)]
pub struct Item {
    /// The maintenance commit.
    pub rev: String,
    pub commit: String,
    pub source: Source,
}

#[derive(Serialize)]
pub struct AssembledReport {
    pub items: Vec<Item>,
    /// Where the assembled commit was checked out, when asked.
    pub checkout: Option<PathBuf>,
}

/// The assembled commit of each of `revs` (HEAD when none are given), and with
/// `checkout`, a detached worktree of the single one at that path.
pub fn assembled(
    root: &Path,
    revs: &[String],
    checkout: Option<&Path>,
    report: &dyn Report,
) -> Result<AssembledReport> {
    if checkout.is_some() && revs.len() > 1 {
        bail!("--checkout takes a single maintenance commit");
    }
    let current = manifest::load(root)?;
    let repo = source::source_repo(root, &current, true, report)?;
    let head = ["HEAD".to_string()];
    let revs = if revs.is_empty() { &head[..] } else { revs };

    let mut items = Vec::new();
    for rev in revs {
        let recipe = Recipe::at(root, rev)?;
        let built = recipe.assembled(root, &repo, report)?;
        recipe.record(&repo, &built.commit)?;
        items.push(Item {
            rev: recipe.rev.clone().expect("a maintenance commit's recipe"),
            commit: built.commit,
            source: built.source,
        });
    }

    let checkout = match checkout {
        Some(path) => {
            let path = std::path::absolute(path)?;
            if path.exists() {
                bail!("{} already exists", path.display());
            }
            git::out(
                &repo,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    &path.to_string_lossy(),
                    &items[0].commit,
                ],
            )?;
            Some(path)
        }
        None => None,
    };
    Ok(AssembledReport { items, checkout })
}
