# fork-fold design

fork-fold maintains a **build recipe for a stack of live branches**: an upstream
base plus an ordered set of topics, each still maintained as a potential
upstream merge target, combined into a single assembled branch with recorded
conflict resolutions. It generalizes the workflow prototyped in
`t3code-assembly`.

## Why not just maintain a branch

A long-lived integration branch conflates the *logical patch* ("this feature,
as a diff against upstream") with the *commit history that happens to encode
it*. Once topics are merged together and fixed up in place, the logical patches
no longer exist as clean objects — they are smeared across merge commits and
cross-topic fixups, and reconciling with upstream becomes archaeology.

## Core invariant

The only durable sources of truth are:

1. **Clean topic branches** — each a minimal diff against upstream, rebased and
   upstreamed independently.
2. **Tracked resolution files** — the recorded rerere pairs for each
   conflicted merge between topics.

The assembled branch is **compiled output**. It is never developed on, never
merged back, and never reconciled with upstream directly. Upstream
reconciliation happens per-topic, where diffs stay small.

Reproducibility is judged by the **tree hash** of the assembled result, not by
commit IDs (generated commits keep real timestamps).

## Concepts

### Remotes

Named git remotes declared in the manifest. Topics may come from any number of
forks; the base usually comes from `upstream`.

### Entries

The manifest is an ordered list of entries. There are exactly three kinds:

- **branch** — `remote:branch`, the normal case: a live topic branch.
- **pr** — a GitHub PR number, sugar for `refs/pull/N/head` on a named remote.
  Convenience for carrying other people's unmerged work.
- **patch** — a tracked patch file applied on top. The escape hatch for
  *semantic* conflicts between topics (e.g. two topics claim the same migration
  number) that have no home on any single branch. A patch entry that exists
  because a merge wasn't resolved properly is a smell; prefer a resolution.

There is no separate "epilogue" phase — an epilogue is just a patch entry that
happens to sit at the end.

### Resolutions

The single resolution mechanism is **git rerere with repo-tracked storage**:

```
resolutions/rerere/<conflict-hash>/preimage    # git's own rr-cache entry format
resolutions/rerere/<conflict-hash>/postimage
resolutions/rerere/INDEX.toml                  # informational: which entry's merge
                                               # produced which hashes
```

Mechanics:

- **Seeding** — every build wipes the build worktree's rr-cache (the source
  repo's shared git-dir `rr-cache/`) and reseeds it *exclusively* from the
  tracked pairs. Nothing ambient leaks in; the operator's own rr-cache is
  never consulted. rerere is enabled only per-command (`-c rerere.enabled=true
  -c rerere.autoUpdate=true` on build operations), never persistently.
- **Auto-resolve** — on a conflicted merge, rerere replays recognized hunks
  and `autoUpdate` stages them. If no unresolved paths remain, the merge is
  committed and the build continues, recording the entry as
  conflicted+auto-resolved in the lock. Otherwise the build stops (exit 2)
  for manual resolution.
- **Harvest** — after `continue` commits a manual resolution, new or updated
  preimage/postimage pairs are copied from the worktree rr-cache into
  `resolutions/rerere/` and attributed in INDEX.toml. The index is for
  auditability only; replay uses the pair directories, verification uses the
  lock's tree hash.
- **Drift tolerance** — rerere keys on the normalized conflict hunks, not on
  tree hashes, so unrelated drift elsewhere (or elsewhere in the same file)
  still auto-resolves. A changed conflict hunk falls back to a manual stop,
  and the new resolution is harvested alongside the old pair.

Known limitation, by design: rerere pairs capture only **conflicted-hunk**
resolutions. Edits made outside conflict hunks while resolving, and conflicts
rerere cannot handle (delete/modify, binary), are not captured — such content
belongs in patch entries. The end-of-build lock tree hash remains the sole
verification invariant, so any uncaptured edit surfaces as a tree mismatch on
reproduction, never as silent drift.

### Manifest and lock

`manifest.toml` is **intent** (which refs are tracked, in what order).
`manifest.lock.json` is **fact** (which OIDs the last build used, the assembled
commit, and its tree hash). Cargo/Bundler semantics.

Concretely the lock has two parts: `pins` (base OID plus per-entry-name OID —
patch entries pin the file's blob hash; moved only by `update` or an entry's
first build) and `build` (the last completed build: `commit`,
`pre_provenance_commit`, `tree` — the pre-provenance content tree, the
reproducibility invariant — `built_tree`, per-entry results with
merged/absorbed/empty/applied status plus conflict/resolution info, and a
snapshot of the manifest entries used to detect prefix-extension). Branch
entries may carry `pr = N` as pure metadata: the PR the branch is published
as, feeding provenance links and `add --prs-from` dedup without changing
merge behavior.

```toml
# manifest.toml
[remotes]
upstream = "https://github.com/example/project"
mine = "https://github.com/colonelpanic8/project"

[base]
remote = "upstream"
ref = "main"

[[entry]]
name = "custom-snooze"          # optional; defaults derived from ref/pr/path
branch = "mine:custom-snooze"

[[entry]]
pr = 3984                       # refs/pull/3984/head on `upstream` by default
remote = "upstream"

[[entry]]
patch = "patches/renumber-migration.patch"
```

### Append machinery / incremental builds

Ordering invariant, designed in deliberately:

> A build whose manifest is a **prefix-extension** of the lock resumes from the
> last assembled commit and processes only the new entries.

Appending never re-touches or re-resolves earlier entries, so "throw stuff on
top" is cheap:

```
fork-fold add mine:some-branch
fork-fold add --pr 4102
fork-fold add --patch fix-thing.patch
fork-fold build          # incremental: merges only the tail
```

Reordering or removing an entry invalidates the suffix from that point and
triggers a rebuild from there — detectable via the lock, never surprising.

## Verbs (v1)

- `build` — assemble the stack from the lock's pinned OIDs (fetching objects
  as needed), auto-resolving conflicts from the tracked rerere pairs. Entries
  not yet in the lock are pinned from live refs on their first build. Stops
  in the build worktree on an unrecognized conflict. `--locked` additionally
  refuses to touch the network or pin anything new.
- `update [ENTRY...]` — the pin bump: repin the base and all entries (or only
  the named ones) to their live remote heads. `build` never moves existing
  pins; `update` is the only verb that does. After a batch bump, `build`
  repairs incrementally — unchanged conflict hunks auto-resolve from the
  tracked pairs, and changed hunks stop for manual resolution entry by entry.
- `continue` — resume after the human resolves a conflict; commits the merge
  and harvests the new pairs into `resolutions/rerere/`.
- `init` — scaffold a maintenance repository: `manifest.toml`, `resolutions/`,
  `patches/`, `flake.nix` (consuming `fork-fold.lib.mkMaintenanceShell`),
  `.envrc`, justfile. `--upstream URL` fills in the base remote; `--submodule`
  additionally adds the upstream as a git submodule at `upstream/` and marks
  it in the manifest as the base-object source (an optional sourcing strategy,
  never a requirement). The same layout is available as a nix flake template
  (`nix flake init -t github:colonelpanic8/fork-fold`).
- `add` — append a branch/pr/patch entry to the manifest. Idempotent: adding
  an entry that is already present is a reported no-op. `--prs-from USER`
  appends every open PR authored by USER on the base repo that is not already
  carried — safe to re-run any time to pick up only the new ones.
- `remove` — remove an entry.
- `prune [--dry-run]` — drop entries whose changes have landed in the base:
  PR entries via the PR's merged state, branch entries via patch-id
  containment (`git cherry`) against the pinned base.
- `status` — lock vs. manifest vs. live refs: what's stale, what an incremental
  build would do, and which entries look merged upstream (prune candidates).

Builds run in a dedicated worktree under `.worktrees/` in the consuming repo.

### Merged topics

When a topic lands upstream, its entry becomes dead weight: once `update`
moves the base past the merge, the entry's merge is an effective no-op (or,
worse, a conflict against its own squash-merged form). `status` flags such
entries and `prune` removes them. Removing an entry invalidates the suffix of
the lock as usual, but after a base bump that contains the merge, the suffix
normally replays cleanly — merged-topic removal and base update belong in the
same repair cycle.

## Consuming repository layout

A "stack repo" that uses fork-fold contains only data:

```
manifest.toml
manifest.lock.json
resolutions/
patches/
```

Publish/install tails (pushing the assembled branch, tagging, Nix repinning,
system activation) are site-specific and live in the consuming repo (justfile,
scripts) — outside fork-fold.

### Nix integration

Beyond `packages` and `devShells`, the fork-fold flake exports:

- `overlays.default` — adds `pkgs.fork-fold`.
- `lib.mkMaintenanceShell { pkgs, extraPackages ? [] }` — dev shell for
  maintenance repos: the compiled fork-fold binary plus `git`, `gh`, and
  `just`. Consuming repos pair it with direnv (`use flake` in `.envrc`).
- `templates.default` — the maintenance-repo scaffold, kept in
  `templates/maintenance/` and shared verbatim with `fork-fold init` via
  `include_str!`.

## Agent-first operation

fork-fold assumes its day-to-day operators are coding agents as much as
humans. Consequences:

- The maintenance template ships `AGENTS.md` (model + invariants +
  operations), a `CLAUDE.md` pointer, and a project skill covering status,
  appends, and the conflict-resolution loop. The skill lives canonically at
  `.agents/skills/fork-fold/` in the open Agent Skills format
  (agentskills.io); `.claude/skills/` and `.codex/skills/` are relative
  symlinks into it, so Claude Code and Codex share one copy and other agents
  find it via AGENTS.md. All of this lives in `templates/maintenance/` so
  `init`, the flake template, and any template mirror cannot drift.
- CLI output is written to be parsed by an agent mid-workflow: explicit
  stopped-state messages naming the worktree and the next command, and
  (planned) `status --json`.

## Implementation

- **Rust**, single static binary, clap CLI, packaged via the flake.
- All git semantics by **shelling out to real `git`** (merge-ort behavior,
  config/credential handling for free). Never libgit2/gitoxide for merges.
- `gh` used only for PR-entry conveniences, and only when a PR entry is
  present.
- Reference semantics: `t3code-assembly/bin/*.py` (the prototype). Its
  hard-won decision to preserve: end-of-build tree verification against the
  lock. Its exact-match full-tree-diff sidecar records were replaced by
  tracked rerere pairs — one mechanism instead of two, with the tree hash
  still catching anything rerere replays wrongly.

## Explicitly deferred (not v1)

- **Groups** — nested manifests whose assembled branch is one entry in a
  parent manifest. Add when a consumer has a repeatedly-conflicting subsystem
  cluster.
- **Freeze/vendor** — hermetic mode: vendored bundles (base + topics) so a
  clone reproduces with no network. The t3code workflow may want this back;
  the default is live refs + lock pins.
- Anything resembling patch algebra. fork-fold pins order and records outcomes;
  it does not try to make patches commute.
