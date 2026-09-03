//! The only module that writes to stdout or stderr.
//!
//! Everything the tool says arrives here as data: a `report::Event` from a
//! build, a verb's result, or the error that ended the command. `Out` turns
//! each into text for a terminal, or into one JSON object per line when the
//! operator asked for `--format json`, so a machine can read the same
//! conversation.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use serde::Serialize;

use crate::add::{AddReport, Admitted, Discovered, ExcludeReport};
use crate::engine::derive::AnchorRule;
use crate::git::short;
use crate::init::InitReport;
use crate::lock::{self, Prefix};
use crate::manifest::edit::{Excluded, Removal};
use crate::manifest::{self, entries_noun};
use crate::ops::{
    CaptureSource, EntryStatus, FixupReport, Flag, ParentStatus, PinMove, PruneReport, Pruned,
    StatusReport, UpdateReport,
};
use crate::report::{Doing, Event, Outcome, Replay, Report, Resolved, StepInfo};
use crate::rerere;

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Format {
    /// Lines for a terminal.
    Text,
    /// One JSON object per line, tagged `event`, `result`, or `error`.
    Json,
}

pub struct Out {
    format: Format,
}

/// A verb's result: what it says on a terminal, and the name it carries as
/// JSON. The data is the type itself; only the words live here.
pub trait Render: Serialize {
    const VERB: &'static str;
    fn text(&self) -> String;
}

#[derive(Serialize)]
struct EventLine<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(flatten)]
    event: &'a Event,
}

#[derive(Serialize)]
struct ResultLine<'a, T: Serialize> {
    #[serde(rename = "type")]
    kind: &'static str,
    verb: &'static str,
    data: &'a T,
}

#[derive(Serialize)]
struct ErrorLine {
    #[serde(rename = "type")]
    kind: &'static str,
    message: String,
    causes: Vec<String>,
}

fn json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("output types serialize")
}

impl Out {
    pub fn new(format: Format) -> Out {
        Out { format }
    }

    pub fn result<T: Render>(&self, value: &T) {
        match self.format {
            Format::Text => println!("{}", value.text()),
            Format::Json => println!(
                "{}",
                json(&ResultLine {
                    kind: "result",
                    verb: T::VERB,
                    data: value,
                })
            ),
        }
    }

    /// Report the error that ended the command; returns the exit code.
    pub fn error(&self, err: &anyhow::Error) -> i32 {
        let causes: Vec<String> = err.chain().skip(1).map(|c| c.to_string()).collect();
        match self.format {
            Format::Text => {
                let mut text = format!("error: {err}");
                for cause in &causes {
                    text.push_str(&format!("\n  caused by: {cause}"));
                }
                eprintln!("{text}");
            }
            Format::Json => println!(
                "{}",
                json(&ErrorLine {
                    kind: "error",
                    message: err.to_string(),
                    causes,
                })
            ),
        }
        1
    }
}

impl Report for Out {
    fn event(&self, event: &Event) {
        match self.format {
            Format::Text => {
                // git's own account of a failed apply belongs on stderr,
                // beside the instructions on stdout.
                if let Event::PatchFailed { stderr, .. } | Event::FixupFailed { stderr, .. } = event
                {
                    eprintln!("{stderr}");
                }
                println!("{}", event_text(event));
            }
            Format::Json => println!(
                "{}",
                json(&EventLine {
                    kind: "event",
                    event,
                })
            ),
        }
    }
}

fn label(step: &StepInfo) -> String {
    format!("[{:2}/{}] {:<24}", step.index + 1, step.total, step.entry)
}

fn file_list(files: &[String]) -> String {
    files
        .iter()
        .map(|file| format!("      {file}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn outcome_text(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Applied => "applied",
        Outcome::AlreadyApplied => "already applied",
    }
}

fn anchor_rule_text(rule: AnchorRule) -> &'static str {
    match rule {
        AnchorRule::PushedReconstruction => {
            "the last reconstruction's parent merge is an ancestor of the pin \
             -- the reconstructed tip was pushed, so everything above it is the \
             entry's own work"
        }
        AnchorRule::Kept => "unchanged: the recorded anchor is still an ancestor of the pin",
        AnchorRule::Detected => {
            "detected: the first-parent walk stopped at the first merge or \
             already-contained commit"
        }
    }
}

fn resolved_text(resolved: &Resolved) -> String {
    match resolved {
        Resolved::StackMerge => "resolved".to_string(),
        Resolved::ParentMerge { parent } if parent.is_empty() => "parent merged".to_string(),
        Resolved::ParentMerge { parent } => format!("parent {parent} merged"),
        Resolved::Replay { commit } => format!("replayed {}", short(commit)),
    }
}

fn doing_text(doing: &Doing) -> String {
    match doing {
        Doing::MergingParent { parent } => format!("merging parent {parent}"),
        Doing::Replaying { commit, subject } => {
            format!("replaying {} ({subject})", short(commit))
        }
    }
}

fn event_text(event: &Event) -> String {
    use Event::*;
    match event {
        Cloning { url, path } => format!("cloning {url} into {}...", path.display()),

        PinnedBase { oid } => format!("  pinned base -> {}", short(oid)),
        Pinned { entry, oid } => format!("  pinned {entry} -> {}", short(oid)),
        PinnedParent { entry, parent, oid } => {
            format!("  pinned {entry}'s parent {parent} -> {}", short(oid))
        }

        UpToDate { tree } => format!("up to date: tree {tree}"),
        Extending { new_entries } => format!(
            "extending the locked build ({new_entries} new {})",
            entries_noun(*new_entries)
        ),
        FromBase { base } => format!("building from base {}", short(base)),
        Seeded { pairs } => format!("seeded {pairs} tracked rerere pair(s)"),
        Resuming { index, total } => format!("resuming at entry {}/{total}", index + 1),

        Applied { step, outcome } => format!("  {} {}", label(step), outcome_text(*outcome)),
        PatchFailed { step, worktree, .. } => format!(
            "\n  {} patch FAILED to apply\n  \
             Resolve in: {}\n  \
             Then: fork-assembler continue",
            label(step),
            worktree.display()
        ),
        Fixup {
            step,
            path,
            outcome,
        } => format!("  {} fixup {path}: {}", label(step), outcome_text(*outcome)),
        FixupFailed {
            step,
            path,
            worktree,
            ..
        } => format!(
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
            step.entry
        ),
        FixupCommitted { step, path } => format!(
            "  {} fixup committed as resolved\n  \
             WARNING: {path} still holds the version that failed; re-capture it with \
             `fork-assembler fixup {} {path} --capture` after this build, or the next \
             rebuild stops here again",
            label(step),
            step.entry
        ),
        Absorbed { step } => format!("  {} ABSORBED upstream -- drop candidate", label(step)),
        Empty { step } => format!(
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
            // entry, and an operator reading the log needs to know which is
            // which.
            let what = if *reconstruction {
                "merged reconstruction"
            } else {
                "merged"
            };
            format!("  {} {what} {}", label(step), short(oid))
        }
        Conflict {
            step,
            files,
            worktree,
        } => format!(
            "\n  {} CONFLICT in {} file(s):\n{}\n\n  \
             Resolve in: {}\n  \
             Stage with `git add`, then: fork-assembler continue",
            label(step),
            files.len(),
            file_list(files),
            worktree.display()
        ),
        AutoResolved { step } => {
            format!("  {} auto-resolved from tracked rerere pairs", label(step))
        }
        Harvested {
            step,
            resolved,
            pairs,
        } => {
            // Harvesting is what makes a resolution durable; one rerere could
            // not capture leaves the next rebuild to stop in exactly the same
            // place, and the operator should hear that now.
            if pairs.is_empty() {
                format!(
                    "  {} {}; WARNING: no rerere pair captured \
                     (unrecognizable conflict) -- a rebuild will stop here again",
                    label(step),
                    resolved_text(resolved)
                )
            } else {
                format!(
                    "  {} {}; harvested {} pair(s) into {}",
                    label(step),
                    resolved_text(resolved),
                    pairs.len(),
                    rerere::DIR,
                )
            }
        }

        Reconstructing {
            step,
            base,
            worktree,
        } => format!(
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
        } => format!(
            "\n  {} CONFLICT {} in {} file(s):\n{}\n\n  \
             Resolve in the DERIVE worktree: {}\n  \
             That worktree holds {}'s reconstruction, not the assembled stack.\n  \
             Stage with `git add`, then: fork-assembler continue",
            label(step),
            doing_text(doing),
            files.len(),
            file_list(files),
            worktree.display(),
            step.entry
        ),
        ParentAbsorbed { step, parent } => format!(
            "  {} parent {parent} ABSORBED -- already in the reconstruction",
            label(step)
        ),
        ParentMerged { step, parent, oid } => {
            format!("  {} parent {parent} merged {}", label(step), short(oid))
        }
        ParentAutoResolved { step, parent } => format!(
            "  {} parent {parent} merged; auto-resolved from tracked rerere pairs",
            label(step)
        ),
        Anchor { step, oid, rule } => format!(
            "  {} anchor {} -- {}",
            label(step),
            short(oid),
            anchor_rule_text(*rule)
        ),
        Delta { step, commits: 0 } => format!(
            "  {} delta: none -- a pure merge of its parents",
            label(step)
        ),
        Delta { step, commits } => format!(
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
            format!(
                "  {} replayed {} ({subject}){suffix}",
                label(step),
                short(commit)
            )
        }
        ReplaySkipped { step, commit } => format!(
            "  {} replayed {}: EMPTY after resolution -- skipped",
            label(step),
            short(commit)
        ),
        PublishSkipped { entry, target } => {
            format!("  {entry} reconstruction publication to {target} skipped (--locked)")
        }
        Published { entry, oid, target } => {
            format!(
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
            let verdict = match previous_tree.as_deref() {
                Some(prev) if prev == tree => {
                    "\ntree UNCHANGED from previous lock -- nothing downstream needs a bump"
                        .to_string()
                }
                Some(prev) => format!("\ntree CHANGED (was {prev})"),
                None => String::new(),
            };
            format!("\ntree:   {tree}\ncommit: {commit}\nconflicts this run: {conflicts}{verdict}")
        }
        LockWritten => format!("wrote {}", lock::FILE),
        Verified => "verified: reproduced the lock's tree exactly".to_string(),
        NothingToVerify => {
            "(no lock to verify against; lock not written in --locked mode)".to_string()
        }
    }
}

// ---- verb results -------------------------------------------------------

fn pin_move_text(what: &str, pin: &PinMove) -> String {
    match &pin.old {
        Some(old) if *old == pin.new => format!("{what}: unchanged ({})", short(&pin.new)),
        Some(old) => format!("{what}: {} -> {}", short(old), short(&pin.new)),
        None => format!("{what}: pinned {}", short(&pin.new)),
    }
}

impl Render for UpdateReport {
    const VERB: &'static str = "update";

    fn text(&self) -> String {
        let mut lines = Vec::new();
        if let Some(base) = &self.base {
            lines.push(format!("  {}", pin_move_text("base", base)));
        }
        for entry in &self.entries {
            lines.push(format!("  {}", pin_move_text(&entry.entry, &entry.pin)));
            for parent in &entry.parents {
                let what = format!("parent {}", parent.parent);
                lines.push(format!("    {}", pin_move_text(&what, &parent.pin)));
            }
            if let Some(anchor) = &entry.anchor {
                lines.push(format!(
                    "    anchor {} -- {}",
                    short(&anchor.oid),
                    anchor_rule_text(anchor.rule)
                ));
            }
        }
        lines.push(format!("wrote {}", lock::FILE));
        lines.join("\n")
    }
}

fn flag_text(flag: &Flag, of_parent: bool) -> String {
    match flag {
        Flag::RerereResolution => "rerere resolution".to_string(),
        Flag::Fixup { path } => format!("fixup {path}"),
        Flag::FixupMissing { path } => format!("fixup {path} MISSING -- build will fail"),
        Flag::LastBuild { status } => {
            format!("last build: {}", status.as_str().to_uppercase())
        }
        Flag::ContainedInBase if of_parent => {
            "absorbed upstream -- consider removing it from parents".to_string()
        }
        Flag::ContainedInBase => "contained in base -- prune candidate".to_string(),
        Flag::ConflictsWithBase => {
            "CONFLICTS WITH BASE -- rebase the topic; build refuses it".to_string()
        }
        Flag::LiveHead { oid } => format!("live head {} -- pin is behind", short(oid)),
    }
}

fn flags_text(flags: &[Flag], of_parent: bool) -> String {
    if flags.is_empty() {
        return String::new();
    }
    let flags: Vec<String> = flags.iter().map(|f| flag_text(f, of_parent)).collect();
    format!("  [{}]", flags.join("; "))
}

fn pin_text(pin: Option<&String>) -> String {
    pin.map_or("UNPINNED".to_string(), |p| short(p).to_string())
}

fn entry_status_text(entry: &EntryStatus, lines: &mut Vec<String>) {
    lines.push(format!(
        "  {:<24} {} ({}){}",
        entry.name,
        pin_text(entry.pin.as_ref()),
        entry.source,
        flags_text(&entry.flags, false)
    ));
    let Some(derived) = &entry.derived else {
        return;
    };
    // A derived entry's parents are not entries and never will be, so they
    // print beneath it: they are part of what this one step reconstructs,
    // and the anchor says where its own work starts.
    for ParentStatus {
        name,
        source,
        pin,
        flags,
    } in &derived.parents
    {
        lines.push(format!(
            "    {:<22} {} ({source}){}",
            format!("parent {name}"),
            pin_text(pin.as_ref()),
            flags_text(flags, true)
        ));
    }
    lines.push(match &derived.anchor {
        Some(anchor) => format!("    {:<22} {}", "anchor", short(anchor)),
        None => format!(
            "    {:<22} UNRESOLVED -- the next build detects it",
            "anchor"
        ),
    });
}

impl Render for StatusReport {
    const VERB: &'static str = "status";

    fn text(&self) -> String {
        let mut lines = Vec::new();
        let base = &self.base;
        lines.push(match &base.pin {
            Some(pin) => format!("base: {} ({})", short(pin), base.source),
            None => format!("base: UNPINNED ({})", base.source),
        });
        if let Some(head) = base
            .live_head
            .as_ref()
            .filter(|h| Some(*h) != base.pin.as_ref())
        {
            lines.push(format!(
                "      live head {} -- run `fork-assembler update base`",
                short(head)
            ));
        }
        for entry in &self.entries {
            entry_status_text(entry, &mut lines);
        }
        // Exclusions are manifest intent with no pin and no step, so they
        // print after the stack rather than in it: what the build will never
        // reach, and what discovery must not re-admit.
        if !self.excludes.is_empty() {
            lines.push("\nexcluded:".to_string());
            for exclusion in &self.excludes {
                lines.push(format!("  {}", exclusion.describe()));
            }
        }
        match &self.last_build {
            Some(build) => {
                lines.push(format!("\nlast build: commit {}", short(&build.commit)));
                lines.push(format!(
                    "  tree {} ({} conflicts)",
                    build.tree, build.conflicts
                ));
                match &build.relation {
                    Prefix::Exact => {
                        lines.push("  manifest matches the lock: `build` is a no-op".to_string())
                    }
                    Prefix::Extension { at } => lines.push(format!(
                        "  manifest extends the lock: `build` merges only entries {}..{} incrementally",
                        at + 1,
                        self.entries.len()
                    )),
                    Prefix::Diverged { reason } => {
                        lines.push(format!("  full rebuild needed: {reason}"))
                    }
                    Prefix::NoBuild => {}
                }
            }
            None => {
                lines.push("\nno completed build recorded; run `fork-assembler build`".to_string())
            }
        }
        lines.join("\n")
    }
}

/// What a removal detached, so nothing silently vanishes from the build.
fn removal_notes(removal: &Removal, lines: &mut Vec<String>) {
    // A coherence fixup repairs an interaction BETWEEN entries, so removing
    // one side does not mean the incoherence is gone — an entry that landed
    // upstream usually still clashes with the topic it clashed with before.
    for orphan in &removal.orphaned_fixups {
        let (name, path) = (&orphan.entry, &orphan.path);
        lines.push(format!(
            "  NOTE: {name} carried the coherence fixup {path}, now unreferenced.\n  \
             {path} is left on disk: if the incoherence it repaired survives the removal, \
             re-home it with `fork-assembler fixup OTHER_ENTRY {path}`; otherwise delete it."
        ));
    }
    // A parent was kept out of the entry list by the declaration that just
    // left with its entry. Unlike a fixup it leaves no file to re-home — it
    // leaves a silence, and a silence is exactly what discovery overwrites.
    for orphan in &removal.orphaned_parents {
        let name = &orphan.entry;
        lines.push(format!(
            "  NOTE: {name} declared {} as parent(s); nothing references them now.\n  \
             Their commits left the stack with {name}. If they are still open PRs, the next \
             `add --prs-from` sweep will offer them again -- carry them as entries if they \
             should be in the stack on their own, or `fork-assembler exclude` them with a reason \
             if they should not.",
            orphan.parents.join(", ")
        ));
    }
}

impl Render for Removal {
    const VERB: &'static str = "remove";

    fn text(&self) -> String {
        let mut lines = vec![format!(
            "entry removed; the build suffix from position {} is invalidated -- run `fork-assembler build`",
            self.earliest + 1
        )];
        removal_notes(self, &mut lines);
        lines.join("\n")
    }
}

impl Render for PruneReport {
    const VERB: &'static str = "prune";

    fn text(&self) -> String {
        let mut lines: Vec<String> = self
            .contained
            .iter()
            .map(|c| format!("  {}: contained in base ({})", c.entry, short(&c.pin)))
            .collect();
        let count = format!(
            "{} {}",
            self.contained.len(),
            entries_noun(self.contained.len())
        );
        match &self.outcome {
            Pruned::Nothing => {
                lines.push("nothing to prune: no pinned entry is contained in the base".to_string())
            }
            Pruned::DryRun => lines.push(format!("would remove {count} (dry run)")),
            Pruned::Removed { removal } => {
                lines.push(format!(
                    "removed {count}; the build suffix from position {} is invalidated -- run `fork-assembler build`",
                    removal.earliest + 1
                ));
                removal_notes(removal, &mut lines);
            }
        }
        lines.join("\n")
    }
}

impl Render for FixupReport {
    const VERB: &'static str = "fixup";

    fn text(&self) -> String {
        match self {
            FixupReport::Detached { entry, index } => format!(
                "detached {entry}'s coherence fixup (the patch file is left in place)\n\
                 entry {} changed -- run `fork-assembler build`",
                index + 1
            ),
            FixupReport::Set {
                entry,
                index,
                path,
                captured,
            } => {
                let mut lines = Vec::new();
                if let Some(source) = captured {
                    let from = match source {
                        CaptureSource::Uncommitted => {
                            "the build worktree's uncommitted changes".to_string()
                        }
                        CaptureSource::Commit { oid, subject } => {
                            format!("commit {} ({subject})", short(oid))
                        }
                    };
                    lines.push(format!("captured {path} from {from}"));
                }
                lines.push(format!("{entry}: coherence fixup set to {path}"));
                lines.push(format!(
                    "entry {} now produces a different tree -- run `fork-assembler build`",
                    index + 1
                ));
                lines.join("\n")
            }
        }
    }
}

impl Render for AddReport {
    const VERB: &'static str = "add";

    fn text(&self) -> String {
        let mut lines = Vec::new();
        for admitted in &self.log {
            match admitted {
                Admitted::Added { target, parents } => {
                    lines.push(format!("added {}", target.label()));
                    for parent in parents {
                        lines.push(format!("  parent: {}", parent.label()));
                    }
                }
                Admitted::AlreadyCarried { target } => {
                    lines.push(format!("already carried: {}", target.label()))
                }
                Admitted::NoOpenPrs { author, slug } => {
                    lines.push(format!("no open PRs by {author} on {slug}"))
                }
                Admitted::Discovered {
                    number: n,
                    title,
                    verdict,
                } => lines.push(match verdict {
                    Discovered::Append => format!("added pr {n} ({title})"),
                    Discovered::AlreadyCarried => format!("already carried: pr {n} ({title})"),
                    Discovered::CarriedAsParent { entry } => {
                        format!("carried as a parent of {entry}: pr {n} ({title}) -- not added")
                    }
                    Discovered::Excluded { reason } => {
                        format!("excluded: pr {n} ({title}) -- not added: {reason}")
                    }
                }),
            }
        }
        lines.push(if self.appended > 0 {
            format!(
                "{} {} appended to {}",
                self.appended,
                entries_noun(self.appended),
                manifest::FILE
            )
        } else {
            "manifest unchanged".to_string()
        });
        lines.join("\n")
    }
}

impl Render for ExcludeReport {
    const VERB: &'static str = "exclude";

    fn text(&self) -> String {
        let label = self.target.label();
        match &self.outcome {
            Excluded::Added => {
                let mut text = format!("excluded {label}");
                if !self.reason_given {
                    text.push_str(
                        "\n  no reason recorded -- add one with `--reason` so the refusal \
                         is still legible later",
                    );
                }
                text.push_str("\n  the lock is untouched; nothing needs rebuilding");
                text
            }
            Excluded::AlreadyRecorded => format!("already excluded: {label}"),
            Excluded::ReasonUpdated { previous } => match previous {
                Some(previous) => {
                    format!("already excluded: {label}\n  reason updated (was: {previous})")
                }
                None => format!("already excluded: {label}\n  reason recorded"),
            },
        }
    }
}

impl Render for InitReport {
    const VERB: &'static str = "init";

    fn text(&self) -> String {
        format!(
            "initialized fork-assembler maintenance repo in {}\n\
             next: edit manifest.toml, `direnv allow`, then `fork-assembler add` / `fork-assembler build`",
            self.dir.display()
        )
    }
}
