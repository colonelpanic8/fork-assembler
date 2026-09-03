//! What a build has to say, separated from how it is said.
//!
//! The engine emits `Event`s and never writes to stdout itself. `Text` is the
//! renderer the CLI uses; anything else that wants to watch a build — a JSON
//! stream, a test harness — implements `Report` and receives the same events.

use std::path::Path;

use crate::engine::Step;
use crate::git::short;
use crate::manifest::entries_noun;
use crate::rerere;

pub trait Report {
    fn event(&self, event: Event<'_>);
}

/// How applying a patch file ended, when it did not fail.
#[derive(Clone, Copy)]
pub enum Outcome {
    Applied,
    AlreadyApplied,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Applied => "applied",
            Outcome::AlreadyApplied => "already applied",
        }
    }
}

/// How replaying one of a derived entry's own commits ended.
#[derive(Clone, Copy)]
pub enum Replay {
    Clean,
    /// Its content was already in the reconstruction; nothing to commit.
    AlreadyPresent,
    /// Conflicted, and tracked rerere pairs resolved every hunk.
    AutoResolved,
}

pub enum Event<'a> {
    // Pinning, before the run starts.
    PinnedBase {
        oid: &'a str,
    },
    Pinned {
        name: &'a str,
        oid: &'a str,
    },
    PinnedParent {
        entry: &'a str,
        parent: &'a str,
        oid: &'a str,
    },

    // How the run starts.
    UpToDate {
        tree: &'a str,
    },
    Extending {
        new_entries: usize,
    },
    FromBase {
        base: &'a str,
    },
    Seeded {
        pairs: usize,
    },
    Resuming {
        index: usize,
        total: usize,
    },

    // One entry's step.
    Applied {
        step: &'a Step<'a>,
        outcome: Outcome,
    },
    PatchFailed {
        step: &'a Step<'a>,
        stderr: &'a str,
        worktree: &'a Path,
    },
    Fixup {
        step: &'a Step<'a>,
        path: &'a str,
        outcome: Outcome,
    },
    FixupFailed {
        step: &'a Step<'a>,
        path: &'a str,
        stderr: &'a str,
        worktree: &'a Path,
    },
    /// `continue` committed a hand-resolved fixup, whose patch file is now
    /// stale.
    FixupCommitted {
        step: &'a Step<'a>,
        path: &'a str,
    },
    Absorbed {
        step: &'a Step<'a>,
    },
    Empty {
        step: &'a Step<'a>,
    },
    Merged {
        step: &'a Step<'a>,
        oid: &'a str,
        reconstruction: bool,
    },
    Conflict {
        step: &'a Step<'a>,
        files: &'a [String],
        worktree: &'a Path,
    },
    AutoResolved {
        step: &'a Step<'a>,
    },
    /// `continue` committed a resolution; `what` names it.
    Harvested {
        step: &'a Step<'a>,
        what: &'a str,
        pairs: &'a [String],
    },

    // Reconstructing a derived entry.
    Reconstructing {
        step: &'a Step<'a>,
        base: &'a str,
        worktree: &'a Path,
    },
    DeriveConflict {
        step: &'a Step<'a>,
        doing: &'a str,
        files: &'a [String],
        worktree: &'a Path,
    },
    ParentAbsorbed {
        step: &'a Step<'a>,
        parent: &'a str,
    },
    ParentMerged {
        step: &'a Step<'a>,
        parent: &'a str,
        oid: &'a str,
    },
    ParentAutoResolved {
        step: &'a Step<'a>,
        parent: &'a str,
    },
    Anchor {
        step: &'a Step<'a>,
        oid: &'a str,
        rule: &'a str,
    },
    Delta {
        step: &'a Step<'a>,
        commits: usize,
    },
    Replayed {
        step: &'a Step<'a>,
        commit: &'a str,
        subject: &'a str,
        outcome: Replay,
    },
    /// `continue` found the resolved pick empty and skipped it.
    ReplaySkipped {
        step: &'a Step<'a>,
        commit: &'a str,
    },
    PublishSkipped {
        entry: &'a str,
        target: &'a str,
    },
    Published {
        entry: &'a str,
        oid: &'a str,
        target: &'a str,
    },

    // The end of a completed run.
    Finished {
        tree: &'a str,
        commit: &'a str,
        conflicts: u32,
        previous_tree: Option<&'a str>,
    },
    LockWritten,
    Verified,
    NothingToVerify,
}

/// The CLI's renderer: one line per event on stdout, with the operator's
/// instructions spelled out wherever a build stops.
pub struct Text;

fn label(step: &Step) -> String {
    format!(
        "[{:2}/{}] {:<24}",
        step.index + 1,
        step.total,
        step.entry.name
    )
}

fn file_list(files: &[String]) -> String {
    files
        .iter()
        .map(|file| format!("      {file}"))
        .collect::<Vec<_>>()
        .join("\n")
}

impl Report for Text {
    fn event(&self, event: Event<'_>) {
        use Event::*;
        match event {
            PinnedBase { oid } => println!("  pinned base -> {}", short(oid)),
            Pinned { name, oid } => println!("  pinned {name} -> {}", short(oid)),
            PinnedParent { entry, parent, oid } => {
                println!("  pinned {entry}'s parent {parent} -> {}", short(oid))
            }

            UpToDate { tree } => println!("up to date: tree {tree}"),
            Extending { new_entries } => println!(
                "extending the locked build ({new_entries} new {})",
                entries_noun(new_entries)
            ),
            FromBase { base } => println!("building from base {}", short(base)),
            Seeded { pairs } => println!("seeded {pairs} tracked rerere pair(s)"),
            Resuming { index, total } => println!("resuming at entry {}/{total}", index + 1),

            Applied { step, outcome } => println!("  {} {}", label(step), outcome.as_str()),
            PatchFailed {
                step,
                stderr,
                worktree,
            } => {
                eprintln!("{stderr}");
                println!(
                    "\n  {} patch FAILED to apply\n  \
                     Resolve in: {}\n  \
                     Then: fork-assembler continue",
                    label(step),
                    worktree.display()
                );
            }
            Fixup {
                step,
                path,
                outcome,
            } => println!("  {} fixup {path}: {}", label(step), outcome.as_str()),
            FixupFailed {
                step,
                path,
                stderr,
                worktree,
            } => {
                eprintln!("{stderr}");
                println!(
                    "\n  {} fixup {path} FAILED to apply\n  \
                     The merge is committed; only the fixup is outstanding.\n  \
                     Resolve the markers in: {}\n  \
                     Then re-capture the corrected fixup and rebuild:\n      \
                     fork-assembler fixup {} {path} --capture\n      \
                     fork-assembler build\n  \
                     Or commit this resolution once, leaving {path} stale:\n      \
                     git add -A && fork-assembler continue",
                    label(step),
                    worktree.display(),
                    step.entry.name
                );
            }
            FixupCommitted { step, path } => println!(
                "  {} fixup committed as resolved\n  \
                 WARNING: {path} still holds the version that failed; re-capture it with \
                 `fork-assembler fixup {} {path} --capture` after this build, or the next \
                 rebuild stops here again",
                label(step),
                step.entry.name
            ),
            Absorbed { step } => println!("  {} ABSORBED upstream -- drop candidate", label(step)),
            Empty { step } => println!(
                "  {} EMPTY -- merge changed nothing, drop candidate",
                label(step)
            ),
            Merged {
                step,
                oid,
                reconstruction,
            } => {
                // Naming the reconstruction is the point: the OID that just
                // landed in the stack is not the pin the lock records for this
                // entry, and an operator reading the log needs to know which
                // is which.
                let what = if reconstruction {
                    "merged reconstruction"
                } else {
                    "merged"
                };
                println!("  {} {what} {}", label(step), short(oid));
            }
            Conflict {
                step,
                files,
                worktree,
            } => println!(
                "\n  {} CONFLICT in {} file(s):\n{}\n\n  \
                 Resolve in: {}\n  \
                 Stage with `git add`, then: fork-assembler continue",
                label(step),
                files.len(),
                file_list(files),
                worktree.display()
            ),
            AutoResolved { step } => {
                println!("  {} auto-resolved from tracked rerere pairs", label(step))
            }
            Harvested { step, what, pairs } => {
                // Harvesting is what makes a resolution durable; one rerere
                // could not capture leaves the next rebuild to stop in exactly
                // the same place, and the operator should hear that now.
                if pairs.is_empty() {
                    println!(
                        "  {} {what}; WARNING: no rerere pair captured \
                         (unrecognizable conflict) -- a rebuild will stop here again",
                        label(step)
                    );
                } else {
                    println!(
                        "  {} {what}; harvested {} pair(s) into {}",
                        label(step),
                        pairs.len(),
                        rerere::DIR,
                    );
                }
            }

            Reconstructing {
                step,
                base,
                worktree,
            } => println!(
                "  {} reconstructing from base {} in {}",
                label(step),
                short(base),
                worktree.display()
            ),
            DeriveConflict {
                step,
                doing,
                files,
                worktree,
            } => println!(
                "\n  {} CONFLICT {doing} in {} file(s):\n{}\n\n  \
                 Resolve in the DERIVE worktree: {}\n  \
                 That worktree holds {}'s reconstruction, not the assembled stack.\n  \
                 Stage with `git add`, then: fork-assembler continue",
                label(step),
                files.len(),
                file_list(files),
                worktree.display(),
                step.entry.name
            ),
            ParentAbsorbed { step, parent } => println!(
                "  {} parent {parent} ABSORBED -- already in the reconstruction",
                label(step)
            ),
            ParentMerged { step, parent, oid } => {
                println!("  {} parent {parent} merged {}", label(step), short(oid))
            }
            ParentAutoResolved { step, parent } => println!(
                "  {} parent {parent} merged; auto-resolved from tracked rerere pairs",
                label(step)
            ),
            Anchor { step, oid, rule } => {
                println!("  {} anchor {} -- {rule}", label(step), short(oid))
            }
            Delta { step, commits: 0 } => println!(
                "  {} delta: none -- a pure merge of its parents",
                label(step)
            ),
            Delta { step, commits } => println!(
                "  {} delta: {commits} commit(s) of its own after the anchor",
                label(step)
            ),
            Replayed {
                step,
                commit,
                subject,
                outcome,
            } => {
                let suffix = match outcome {
                    Replay::Clean => "",
                    Replay::AlreadyPresent => ": EMPTY -- already present",
                    Replay::AutoResolved => "; auto-resolved from tracked rerere pairs",
                };
                println!(
                    "  {} replayed {} ({subject}){suffix}",
                    label(step),
                    short(commit)
                );
            }
            ReplaySkipped { step, commit } => println!(
                "  {} replayed {}: EMPTY after resolution -- skipped",
                label(step),
                short(commit)
            ),
            PublishSkipped { entry, target } => {
                println!("  {entry} reconstruction publication to {target} skipped (--locked)")
            }
            Published { entry, oid, target } => {
                println!(
                    "  {entry} published reconstruction {} -> {target}",
                    short(oid)
                )
            }

            Finished {
                tree,
                commit,
                conflicts,
                previous_tree,
            } => {
                let verdict = match previous_tree {
                    Some(prev) if prev == tree => {
                        "\ntree UNCHANGED from previous lock -- nothing downstream needs a bump"
                            .to_string()
                    }
                    Some(prev) => format!("\ntree CHANGED (was {prev})"),
                    None => String::new(),
                };
                println!(
                    "\ntree:   {tree}\ncommit: {commit}\nconflicts this run: {conflicts}{verdict}"
                );
            }
            LockWritten => println!("wrote {}", crate::lock::FILE),
            Verified => println!("verified: reproduced the lock's tree exactly"),
            NothingToVerify => {
                println!("(no lock to verify against; lock not written in --locked mode)")
            }
        }
    }
}
