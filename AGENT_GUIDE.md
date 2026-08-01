# Operating a fork-fold stack

Read `AGENTS.md` in the repository root first. It states the model and
repository-specific invariants. The short version: `manifest.toml` is intent,
`manifest.lock.json` is fact, the assembled branch is disposable compiled
output, and conflict resolutions live only in tracked files under
`resolutions/`.

## Reporting status

Run `fork-fold status` for an offline comparison of the manifest, lock, and
last build. It also lists the manifest's exclusions with their reasons, after
the stack. Add `--live` when current remote heads or merged PR state matter.
Report whether live refs moved past their lock pins, whether the manifest is a
prefix-extension of the lock (append = cheap incremental build; reorder or
removal = suffix rebuild with likely re-resolution), and whether the last
build completed.

## Appending to the stack

1. Run `fork-fold add REMOTE:BRANCH` (or `--pr N`, or `--patch FILE`). For a
   new fork, first add it under `[remotes]` in `manifest.toml`. To carry all
   of a user's open PRs, run `fork-fold add --prs-from USER`; it is
   idempotent and appends only PRs that are neither already carried nor
   excluded.
2. Run `fork-fold build`. Appends build incrementally from the last assembled
   commit.
3. If it completes, commit `manifest.toml`, `manifest.lock.json`, and any
   tracked resolution changes together using explicit paths. Unrelated local
   changes may exist.

## Refusing a target

Absence records nothing. `add --prs-from` re-appends every open PR it finds,
so deleting an entry — or commenting it out — is not a decision the manifest
remembers. To keep a target out, say so:

```sh
fork-fold exclude --pr 3970 --reason "superseded by 3984, which contains it"
```

The case that forces this used to be a combined PR that merges two others and
builds on top of both. That case now has a better answer — declare the parents,
see below — and an exclusion is what remains for targets nothing carries:
abandoned PRs, work superseded by an upstream change, anything a sweep would
otherwise offer forever.

Always record a `--reason`. An exclusion nobody can justify later is
indistinguishable from an oversight, and the reason is quoted wherever the
refusal is reported.

`exclude` never touches the lock — nothing needs rebuilding after one. It
refuses a target that is currently carried: run `fork-fold remove NAME` first
and accept that it invalidates the build from that position. Report both the
exclusion and any removal it required.

Exclude a closed or superseded PR too, even though discovery only sees open
ones. The exclusion is what answers "why isn't this carried?" without git
archaeology.

## Derived entries: carrying a combined PR

When an entry merges other PRs and adds work on top, say so instead of
excluding them:

```sh
fork-fold add --pr 4102 --parent 2525 --parent mine:auth-refactor
```

That writes `parents = [{ pr = 2525 }, { branch = "mine:auth-refactor" }]` on
the entry, in the order it merged them. The entry is now **derived**, and:

- Discovery skips those PRs with "carried as a parent of ENTRY". Do not also
  add or exclude them — the manifest rejects both, because the parent
  declaration already keeps them out and says why.
- `build` reconstructs the entry: it re-merges each parent onto the pinned base
  in a second worktree (`.worktrees/derive`), replays the entry's own commits —
  its *delta* — on top, and merges that result into the stack instead of the
  entry's pin. The lock still records the pin as what the entry tracks.
- `update` repins each parent alongside the entry, then re-establishes the
  **anchor**: the commit in the entry's history after which its own commits
  start. Report the anchor line it prints. It names which of three rules fired
  — the reconstructed tip was pushed, the recorded anchor still holds, or the
  boundary was detected by a first-parent walk — and that line is the only
  audit of what will be replayed as the entry's own work.
- `status` lists the parents indented under their entry with their pins, flags
  a parent the base has absorbed ("consider removing it from parents"), and
  shows the anchor.

The entry's branch must genuinely *merge* its parents. One that cherry-picks or
rebases them onto itself has no boundary to anchor on, and the build says so
rather than guessing. Delta commits that are themselves merges are skipped.

If you push the reconstructed tip to the entry's branch, say so in the report:
the next `update` will notice (rule 1) and treat everything above it as the
entry's own work, which is what makes review commits on a published
reconstruction work without duplicating anything.

## When a build stops on a conflict

1. Go to the build worktree reported by the command, under `.worktrees/`.
2. Resolve the conflicted files. Judge each resolution against what the topic
   and the earlier stack each intend; do not mechanically prefer one side. If
   the correct fix belongs within one topic, fix that topic branch instead
   and rebuild.
3. Stage the resolved files with `git add`, then run `fork-fold continue`
   from the maintenance repository root.
4. Commit the new tracked pairs under `resolutions/rerere/`, their
   informational `INDEX.toml`, the manifest, and the lock together.

A stop inside a reconstruction says **DERIVE worktree** and names
`.worktrees/derive`. Resolve there, not in the build worktree: the build
worktree is paused behind the reconstruction and holds nothing to fix. The loop
is otherwise identical — `git add`, then `fork-fold continue`, which finishes
the parent merge or the cherry-pick, harvests the pair under the *entry's*
name, and carries on through the rest of the reconstruction and into the stack
merge. Judge these resolutions against what each parent intends; the combined
PR's author already made this decision once, so prefer reproducing their
resolution over inventing a new one.

## Coherence fixups

Rerere pairs capture only conflicted hunks. When correctness needs something
those pairs cannot hold — an edit outside every conflict hunk, or a semantic
clash between topics that is textually clean (two topics claiming one
migration number) — record it as the entry's **coherence fixup**, not as a
patch entry sitting later in the order:

```sh
fork-fold fixup ENTRY patches/thing.patch --capture   # from the build worktree
fork-fold build
```

Attach it to the entry whose admission caused the problem — normally the
later of the interacting entries. It is then applied inside that entry's own
step, right after its merge, so no entry boundary is ever an invalid tree.
Reserve standalone `--patch` entries for content that belongs to no entry at
all, such as site-local customization.

`--capture` writes the patch from the build worktree: its uncommitted changes
when there are any (at a fixup stall, precisely the corrected patch), else the
entry's existing `fork-fold: fixup ENTRY` commit. Fixups are not pinned, so
editing one needs no `update` — the next `build` picks it up.

When a build stops on a fixup that no longer applies, the entry's merge is
already committed and only the fixup is outstanding. Repair the worktree,
re-capture, and rebuild. You may instead `git add` and `fork-fold continue` to
commit that resolution once, but the patch file then no longer describes what
shipped and the next rebuild stops in the same place — re-capture afterwards.

When `remove` or `prune` reports an orphaned fixup, decide explicitly: a fixup
repairs an interaction *between* entries, so a topic landing upstream usually
does not dissolve the incoherence. Re-home the patch onto the surviving entry
or delete it; do not leave the question open.

## Bumping to current upstream

`build` never moves an existing pin. To refresh:

1. Run `fork-fold update` for all pins or `fork-fold update ENTRY...` for a
   selective bump.
2. Run `fork-fold build`. Known conflict hunks replay from the tracked pairs;
   changed conflicts stop for resolution.
3. After the repair completes, run `fork-fold build --locked` to prove the
   tracked inputs reproduce the lock's tree.
4. Commit the manifest, lock, and tracked resolution changes together.

## When a PR merges upstream

Run `fork-fold status --live` to identify entries contained in the updated
base. In the same repair cycle, update the base past the merge, run
`fork-fold prune --dry-run`, prune the dead entries, and rebuild. Report which
entries were pruned and why, and resolve any orphaned fixups the prune
reported.

Pruning a derived entry also reports its parents: they left the stack with it,
and the declaration that kept discovery away from them left too. Decide
explicitly whether each parent should now be carried as an entry of its own or
excluded with a reason. When only a *parent* lands upstream, nothing prunes —
`status` flags it as absorbed, and the right move is to drop it from the
`parents` list, which is a manifest edit and a rebuild.

The same applies when the base picks up someone else's fix for a problem a
carried entry also solves: the entry is now redundant, not merged, so `prune`
will not see it. Remove it, exclude it so a sweep cannot bring it back, and
say in the reason which upstream change obsoleted it.

## Verification

After a completed repair, `fork-fold build --locked` must reproduce the
lock's tree hash. Never enable persistent Git rerere, develop on the assembled
branch, or merge from the assembled branch.
