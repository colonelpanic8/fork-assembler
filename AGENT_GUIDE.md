# Operating a fork-fold stack

Read `AGENTS.md` in the repository root first. It states the model and
repository-specific invariants. The short version: `manifest.toml` is intent,
`manifest.lock.json` is fact, the assembled branch is disposable compiled
output, and conflict resolutions live only in tracked files under
`resolutions/`.

## Reporting status

Run `fork-fold status` for an offline comparison of the manifest, lock, and
last build. Add `--live` when current remote heads or merged PR state matter.
Report whether live refs moved past their lock pins, whether the manifest is a
prefix-extension of the lock (append = cheap incremental build; reorder or
removal = suffix rebuild with likely re-resolution), and whether the last
build completed.

## Appending to the stack

1. Run `fork-fold add REMOTE:BRANCH` (or `--pr N`, or `--patch FILE`). For a
   new fork, first add it under `[remotes]` in `manifest.toml`. To carry all
   of a user's open PRs, run `fork-fold add --prs-from USER`; it is
   idempotent and appends only PRs not already carried.
2. Run `fork-fold build`. Appends build incrementally from the last assembled
   commit.
3. If it completes, commit `manifest.toml`, `manifest.lock.json`, and any
   tracked resolution changes together using explicit paths. Unrelated local
   changes may exist.

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

Rerere pairs capture only conflicted hunks. If correctness requires an edit
outside those hunks, put that change in a tracked patch entry; the tree
invariant will otherwise expose the missing edit.

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
entries were pruned and why.

## Verification

After a completed repair, `fork-fold build --locked` must reproduce the
lock's tree hash. Never enable persistent Git rerere, develop on the assembled
branch, or merge from the assembled branch.
