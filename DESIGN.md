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
- **patch** — a standalone tracked patch file applied at its own position.
  This is for content that genuinely belongs to no entry: site-local
  customization that is not upstreamable and not anyone's merge fallout. A
  patch entry that exists because a merge wasn't resolved properly is a smell;
  prefer a resolution or a fixup.

There is no separate "epilogue" phase — an epilogue is just a patch entry that
happens to sit at the end.

### Exclusions

An exclusion names a target — the same three shapes an entry can take — that
the stack deliberately does not carry.

It exists because absence records nothing. Discovery (`add --prs-from`)
appends every open PR it finds, so deleting an entry or commenting it out is
not a decision the manifest remembers: the next sweep puts it back. The
motivating case is a combined PR that merges two others and builds on both —
carrying either parent alongside it duplicates that parent's commits, and the
parents stay open, so they keep resurfacing. An exclusion is the positive
statement that a target must stay out, and the only one a sweep will honor.

Exclusions are intent with no step: no pin, no position, no fixup, no effect
on any assembled tree. They constrain what may enter the entry list, nothing
more. Carrying and excluding the same target is therefore not a precedence
question — it is two contradictory statements of intent, and `load` rejects
it rather than picking a winner.

A `reason` is optional and not load-bearing, but recording one is most of the
point: an exclusion nobody can justify six months later is indistinguishable
from an oversight. It is quoted wherever the refusal is reported.

### Coherence fixups

Branch and pr entries may additionally carry `fixup = "patches/thing.patch"`:
a tracked patch applied **inside that entry's own step**, immediately after
its merge commits, before the next entry merges.

A fixup exists because admitting this entry alongside the ones before it
breaks something no single branch owns — two topics claiming one migration
number, a resolution needing an edit outside every conflict hunk. That is the
same event as a conflicted merge, so it gets the same home: bound to the
entry, harvested and replayed as part of its step. The `patch` entry kind
stays for standalone content; a fixup is not a peer of the entries around it.

Binding it there buys three things a trailing patch entry cannot:

- **Every entry boundary is a coherent tree.** The invariant is per-entry, not
  per-commit (the merge commit inside a step can still be momentarily
  invalid) — which is exactly the granularity the lock and prefix-extension
  already work at.
- **The dependency is machine-readable.** `remove` and `prune` can see that a
  patch existed to reconcile *this* entry and report it, instead of leaving an
  anonymous patch entry to fail to apply — or, worse, apply cleanly and do
  something no longer meaningful.
- **One repair loop.** A build stops the same way for an unrecognized conflict
  and for a fixup that no longer applies, and `fork-fold fixup ENTRY PATH
  --capture` turns whatever the human did in the build worktree back into the
  tracked patch.

Fixups are **not pinned**. Their blob hash rides in the lock's manifest
snapshot, so editing one invalidates from its entry exactly as repinning that
entry would, and the next `build` picks it up with no `update` step — a fixup
is repo-local content, not a tracked remote ref.

Known limitation, by design: a fixup is really owned by an **edge**, not a
node. "These two topics both claim migration 0042" is a property of the pair;
attaching it to the later of the two is the only well-defined choice given
manifest order, and it is visibly an approximation when that entry lands
upstream and gets pruned while the incoherence survives. So removal never
deletes a fixup file — it reports it as orphaned and leaves re-homing to the
human. Better than a silent orphan; not the same as tracking the pair.

Attachment *expresses* coherence; it does not *enforce* it. Nothing verifies
that an entry boundary actually builds — a textually clean merge that is
semantically broken still sails through, and the fixup mechanism only gives
the eventual repair a correct home. Enforcement would be a per-entry check
hook; see "Explicitly deferred".

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
belongs in the entry's coherence fixup, which is applied in the same step as
the resolution it completes. The end-of-build lock tree hash remains the sole
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
snapshot of the manifest entries used to detect prefix-extension, each
carrying its entry's fixup blob hash when it has one). Branch entries may
carry `pr = N` as pure metadata: the PR the branch is published as, feeding
provenance links and `add --prs-from` dedup without changing merge behavior.

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
fixup = "patches/renumber-migration.patch"   # applied inside THIS entry's step

[[entry]]
patch = "patches/site-local-branding.patch"  # standalone, at its own position

[[exclude]]
pr = 3970                       # deliberately not carried; discovery skips it
reason = "superseded by 3984, which already contains it"
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
  and harvests the new pairs into `resolutions/rerere/`, then runs that
  entry's fixup, since a resolution and its fixup are one step. Also resumes
  a build stopped on a fixup that no longer applies, committing only the
  fixup — the merge already happened and is never replayed.
- `fixup ENTRY [PATH]` — attach a coherence fixup to an entry, `--capture` it
  from the build worktree (its uncommitted changes, which at a fixup stall are
  exactly the corrected patch; otherwise the entry's existing fixup commit),
  or `--remove` to detach it. Attaching or editing a fixup changes what that
  entry's step produces, so the next `build` redoes it.
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
  carried or excluded — safe to re-run any time to pick up only the new ones.
  Naming an excluded target explicitly is an error rather than a silent skip:
  a sweep is impersonal, but an explicit add is a decision, and it deserves to
  learn that it contradicts a recorded refusal.
- `exclude` — record a target as deliberately not carried. Idempotent, and it
  never touches the lock, so nothing needs rebuilding after one. Refuses a
  target that is currently carried: dropping it invalidates every later
  entry's build, and that consequence belongs to `remove`, which reports it.
- `remove` — remove an entry. Reports any coherence fixup it carried, leaving
  the patch file on disk for re-homing.
- `prune [--dry-run]` — drop entries whose changes have landed in the base:
  PR entries via the PR's merged state, branch entries via patch-id
  containment (`git cherry`) against the pinned base. Reports orphaned fixups
  the same way `remove` does — a topic landing upstream rarely dissolves the
  incoherence its fixup repaired.
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
- `lib.agentGuide` — the authoritative agent operating guide as text.
  Maintenance flakes re-export it from their pinned `fork-fold` input, letting
  a stable local discovery stub load version-matched instructions with
  `nix eval` rather than vendoring a copy.

## Agent-first operation

fork-fold assumes its day-to-day operators are coding agents as much as
humans. Consequences:

- The maintenance template ships `AGENTS.md` (model + invariants +
  operations), a `CLAUDE.md` pointer, and a project skill covering status,
  appends, and the conflict-resolution loop. The skill lives canonically at
  `.agents/skills/fork-fold/` in the open Agent Skills format
  (agentskills.io); `.claude/skills/` and `.codex/skills/` are relative
  symlinks into it, so Claude Code and Codex share one entry point and other
  agents find it via AGENTS.md. That local skill is a small, stable discovery
  stub. It evaluates `lib.forkFoldAgentGuide`, re-exported directly from the
  repository's pinned `fork-fold` input, so the substantive instructions
  cannot drift independently of the tool. The unavoidable local stub contains
  only discovery metadata and that loading protocol.
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

- **Per-entry checks** — the enforcement half of coherence fixups: a
  `check = "cargo test"` run at each entry boundary during `build`, turning
  "every boundary is coherent" from a convention into a verified property.
  Deferred because it is O(n) test runs per assembly; it should be opt-in
  (`build --check`) and ideally skip boundaries whose inputs did not move.
- **Residue harvesting** — auto-capturing the out-of-hunk part of a manual
  resolution into the entry's fixup. Computable: replay the merge with the
  harvested rerere pairs seeded and diff against what was actually committed;
  whatever differs is the residue. Deferred because a wrong auto-generated
  patch is worse than an absent one. Today `continue` warns instead, and
  `fixup --capture` makes recording it a single command.
- **Groups** — nested manifests whose assembled branch is one entry in a
  parent manifest. Add when a consumer has a repeatedly-conflicting subsystem
  cluster.
- **Freeze/vendor** — hermetic mode: vendored bundles (base + topics) so a
  clone reproduces with no network. The t3code workflow may want this back;
  the default is live refs + lock pins.
- Anything resembling patch algebra. fork-fold pins order and records outcomes;
  it does not try to make patches commute.
