//! The base-conflict refusal.
//!
//! Resolutions are only ever recorded for conflicts *between* carried topics.
//! A topic that cannot merge with the base on its own is out of date with
//! upstream, and `build` refuses it instead of offering to resolve it — the
//! fix is a rebase on the topic branch, pushed where its author and reviewers
//! can see it.

use std::path::Path;

use anyhow::Result;

use super::Ctx;
use crate::git;
use crate::manifest::{Entry, Kind, Parent};
use crate::state;

/// The files that conflict when `oid` is merged with the base ALONE — nothing
/// else in the stack involved. A non-empty answer means the topic is simply out
/// of date with upstream, which is a fact about the topic and not about this
/// assembly.
///
/// `merge-tree --write-tree` answers this without a worktree, using the same
/// merge machinery the real merge will use.
fn base_conflict_files(repo: &Path, base: &str, oid: &str) -> Result<Vec<String>> {
    let out = git::raw(
        repo,
        &["merge-tree", "--write-tree", "--name-only", base, oid],
    )?;
    // 0 = merged clean, 1 = conflicts. Anything else means merge-tree could not
    // answer the question at all (unrelated histories, a missing object), and
    // an unanswered question is not a base conflict.
    if out.status.code() != Some(1) {
        return Ok(Vec::new());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(stdout
        .lines()
        .skip(1) // the merged tree's OID
        .take_while(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect())
}

/// True when `oid` cannot merge with the base on its own. For reporting, where
/// only the answer matters and a merge-tree that cannot answer means "no".
pub fn conflicts_with_base(repo: &Path, base: &str, oid: &str) -> bool {
    base_conflict_files(repo, base, oid).is_ok_and(|files| !files.is_empty())
}

/// Roll the build back to nothing in progress.
///
/// A base conflict is not a stop the operator resumes from — the repair happens
/// in a different repository, on the topic branch — so leaving a half-merged
/// worktree and a state file behind would only make the next `build` refuse to
/// start for an unrelated-looking reason.
fn abandon(ctx: &Ctx) {
    for worktree in [ctx.worktree.clone(), ctx.derive_worktree()] {
        if worktree.exists() {
            let _ = git::raw(&worktree, &["merge", "--abort"]);
            let _ = git::raw(&worktree, &["cherry-pick", "--abort"]);
        }
    }
    let _ = state::clear(&ctx.worktree);
}

/// What is being checked against the base: an entry's own pin, or one parent of
/// a derived entry. Both are topics somebody maintains elsewhere, which is the
/// only thing the refusal needs to know about them.
pub struct Topic<'a> {
    /// How to name it to the operator.
    label: String,
    /// What it tracks, as `status` and the lock spell it.
    source: String,
    /// Whether it is a pull request, which changes where the fix gets pushed.
    is_pr: bool,
    oid: &'a str,
}

impl<'a> Topic<'a> {
    pub fn entry(entry: &Entry, oid: &'a str) -> Topic<'a> {
        Topic {
            label: entry.name.clone(),
            source: entry.source(),
            is_pr: matches!(entry.kind, Kind::Pr { .. }),
            oid,
        }
    }

    pub fn parent(entry: &Entry, parent: &Parent, oid: &'a str) -> Topic<'a> {
        Topic {
            label: format!("{}: parent {}", entry.name, parent.name),
            source: parent.source(),
            is_pr: matches!(parent.kind, Kind::Pr { .. }),
            oid,
        }
    }
}

/// The refusal.
///
/// A topic that conflicts with the base conflicts with nothing this repository
/// owns, so nothing this repository can record is a fix. Recording a resolution
/// here would hide a broken topic from its own author and reviewers, and would
/// have to be re-resolved on every base bump — the conflict comes back with the
/// next upstream commit that touches those files, forever.
fn base_conflict_error(
    ctx: &Ctx,
    entry: &Entry,
    topic: &Topic,
    base: &str,
    files: &[String],
) -> anyhow::Error {
    let Topic {
        label,
        source,
        is_pr,
        oid,
    } = topic;
    let repo = ctx.repo.display();
    let push_hint = if *is_pr {
        "  # then push the result to the PR's head branch. If the PR is someone\n  \
         # else's, ask its author to rebase, or `fork-assembler exclude` it with\n  \
         # that as the reason -- do not carry a topic that no longer applies."
    } else {
        "  # then push the result to the branch this entry tracks (force-with-lease)."
    };
    anyhow::anyhow!(
        "{label} conflicts with the BASE ITSELF, not with anything else in the stack.\n\
         \n  \
         base    {} ({}:{})\n  \
         topic   {} ({source})\n  \
         file(s) {}\n\
         \n\
         This is not an assembly conflict, and it will not be resolved here. A tracked \
         resolution would paper over a topic that no longer applies to upstream: it would \
         hide the breakage from the topic's own author and reviewers, and it would have to \
         be re-resolved every time the base moves.\n\
         \n\
         Fix it where it lives. Bring the topic up to date against the base, resolve the \
         conflict there, and publish that:\n\
         \n  \
         git -C {repo} checkout -B onto-base {}\n  \
         git -C {repo} rebase {}\n\
         {push_hint}\n\
         \n\
         Then `fork-assembler update {}` and build again.\n\
         \n\
         This build has been rolled back; nothing is left in progress.",
        git::short(base),
        ctx.manifest.base.remote,
        ctx.manifest.base.ref_,
        git::short(oid),
        files.join(", "),
        git::short(oid),
        git::short(base),
        entry.name,
    )
}

/// Refuse the build outright when `topic` cannot merge with the base on its
/// own. Called the moment a merge conflicts, before any resolution — recorded
/// or manual — gets a chance to obscure why.
pub fn refuse_if_base_conflict(ctx: &Ctx, entry: &Entry, topic: Topic, base: &str) -> Result<()> {
    let files = base_conflict_files(&ctx.repo, base, topic.oid)?;
    if files.is_empty() {
        return Ok(());
    }
    abandon(ctx);
    Err(base_conflict_error(ctx, entry, &topic, base, &files))
}
