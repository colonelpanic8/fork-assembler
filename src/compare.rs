//! `diff`: a git diff between the assembled branches two maintenance commits
//! built.
//!
//! Each side is materialized as `assembled` does it, so either may be an old
//! maintenance commit whose assembled commit is long gone. The diff is taken
//! between the content commits beneath the provenance record, which changes
//! with every recipe and says nothing about the stack.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{bail, Result};
use serde::Serialize;

use crate::git;
use crate::manifest;
use crate::recipe::{Assembled, Recipe};
use crate::report::Report;
use crate::source;

#[derive(Serialize)]
pub struct Side {
    /// The maintenance commit, or "working tree".
    pub recipe: String,
    #[serde(flatten)]
    pub assembled: Assembled,
}

#[derive(Serialize)]
pub struct DiffReport {
    pub from: Side,
    pub to: Side,
    /// git's output, verbatim.
    pub diff: String,
}

/// Diff the assembled branch built at `from` (default HEAD) against the one
/// built at `to` (default the working tree). `carried` limits the diff to the
/// paths either side's stack changes, leaving out what moved only upstream;
/// `git_args` go to `git diff` as they are.
pub fn diff(
    root: &Path,
    from: Option<&str>,
    to: Option<&str>,
    carried: bool,
    git_args: &[String],
    report: &dyn Report,
) -> Result<DiffReport> {
    if carried && git_args.iter().any(|arg| arg == "--") {
        bail!("--carried chooses the paths itself; it cannot be combined with a pathspec");
    }
    let current = manifest::load(root)?;
    let repo = source::source_repo(root, &current, true, report)?;
    let side = |recipe: Recipe| -> Result<Side> {
        let assembled = recipe.assembled(root, &repo, report)?;
        recipe.record(&repo, &assembled.commit)?;
        Ok(Side {
            recipe: recipe.label(),
            assembled,
        })
    };
    let from = side(Recipe::at(root, from.unwrap_or("HEAD"))?)?;
    let to = side(match to {
        Some(rev) => Recipe::at(root, rev)?,
        None => Recipe::working(root)?,
    })?;

    let (a, b) = (
        &from.assembled.pre_provenance_commit,
        &to.assembled.pre_provenance_commit,
    );
    let paths = if carried {
        let mut paths = touched(&repo, &from.assembled.base, a)?;
        paths.extend(touched(&repo, &to.assembled.base, b)?);
        paths
    } else {
        BTreeSet::new()
    };
    // The caller's options go before the two commits and their pathspec after,
    // where git expects each.
    let split = git_args.iter().position(|arg| arg == "--");
    let (options, pathspec) = git_args.split_at(split.unwrap_or(git_args.len()));
    let mut args: Vec<&str> = vec!["diff"];
    args.extend(options.iter().map(String::as_str));
    args.extend([a.as_str(), b.as_str()]);
    if carried {
        args.push("--");
        args.extend(paths.iter().map(String::as_str));
    } else {
        args.extend(pathspec.iter().map(String::as_str));
    }
    let diff = if carried && paths.is_empty() {
        String::new()
    } else {
        String::from_utf8_lossy(&git::bytes(&repo, &args)?).to_string()
    };
    Ok(DiffReport { from, to, diff })
}

/// The paths the stack changes on top of its base.
fn touched(repo: &Path, base: &str, assembled: &str) -> Result<BTreeSet<String>> {
    let out = git::bytes(
        repo,
        &["diff", "--name-only", "--no-renames", "-z", base, assembled],
    )?;
    Ok(out
        .split(|b| *b == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).to_string())
        .collect())
}
