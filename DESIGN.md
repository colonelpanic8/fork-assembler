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
2. **Tracked resolution files** — the recorded outcome of each conflicted
   merge between topics.

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

When merging entry *E* conflicts, the resolution is stored as a tracked sidecar
pair keyed by entry name:

```
resolutions/<entry>.toml    # exact-match key: first-parent tree, topic OID, resolved tree
resolutions/<entry>.patch   # binary-safe full diff: first-parent tree -> resolved tree
```

Properties:

- The patch is a **full tree diff** (first parent → resolved merge), including
  cleanly-merged paths, so replay never depends on conflict-marker placement or
  fuzzy patch context.
- **Exact match** (parent tree + topic OID both match): apply the patch,
  verify the resulting tree hash, continue non-interactively.
- **Stale** (inputs drifted because upstream or a topic moved): replay the old
  patch as a 3-way merge and stop with the result staged as a *proposal*; the
  human confirms or fixes, and `fork-fold continue` rewrites the sidecar files
  in place. Resolutions self-maintain across refreshes instead of invalidating.
- Git `rerere` is explicitly disabled during builds — resolutions must come
  from tracked files only, never from a machine-local cache.

### Manifest and lock

`manifest.toml` is **intent** (which refs are tracked, in what order).
`manifest.lock.json` is **fact** (which OIDs the last build used, the assembled
commit, and its tree hash). Cargo/Bundler semantics.

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

- `build` — fetch tracked refs, check out the base, merge entries in order,
  applying tracked resolutions (exact or proposed) as needed. Stops in the
  build worktree on an unresolved conflict. `--locked` builds from the lock's
  OIDs without fetching.
- `continue` — resume after the human resolves/confirms a conflict; records or
  rewrites the resolution sidecar files.
- `init` — scaffold a maintenance repository: `manifest.toml`, `resolutions/`,
  `patches/`, `flake.nix` (consuming `fork-fold.lib.mkMaintenanceShell`),
  `.envrc`, justfile. `--upstream URL` fills in the base remote; `--submodule`
  additionally adds the upstream as a git submodule at `upstream/` and marks
  it in the manifest as the base-object source (an optional sourcing strategy,
  never a requirement). The same layout is available as a nix flake template
  (`nix flake init -t github:colonelpanic8/fork-fold`).
- `add` — append a branch/pr/patch entry to the manifest.
- `remove` — remove an entry.
- `status` — lock vs. manifest vs. live refs: what's stale, what an incremental
  build would do.

Builds run in a dedicated worktree under `.worktrees/` in the consuming repo.

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
  hard-won decisions to preserve: exact input matching before non-interactive
  replay, rerere disabled, full-tree-diff resolutions, tree verification after
  every replayed merge.

## Explicitly deferred (not v1)

- **Groups** — nested manifests whose assembled branch is one entry in a
  parent manifest. Add when a consumer has a repeatedly-conflicting subsystem
  cluster.
- **Freeze/vendor** — hermetic mode: vendored bundles (base + topics) so a
  clone reproduces with no network. The t3code workflow may want this back;
  the default is live refs + lock pins.
- Anything resembling patch algebra. fork-fold pins order and records outcomes;
  it does not try to make patches commute.
