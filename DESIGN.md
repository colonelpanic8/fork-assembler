# fork-assembler design

fork-assembler maintains a **build recipe for a stack of live branches**: an upstream
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
(For that particular case there is now a better answer than an exclusion:
declare the parents. See "Derived entries" below.)

Exclusions are intent with no step: no pin, no position, no fixup, no effect
on any assembled tree. They constrain what may enter the entry list, nothing
more. Carrying and excluding the same target is therefore not a precedence
question — it is two contradictory statements of intent, and `load` rejects
it rather than picking a winner.

A `reason` is optional and not load-bearing, but recording one is most of the
point: an exclusion nobody can justify six months later is indistinguishable
from an oversight. It is quoted wherever the refusal is reported.

### Derived entries

A **derived** entry is one that declares what it merged in:

```toml
[[entry]]
pr = 4102
parents = [{ pr = 2525 }, { branch = "mine:auth-refactor" }]
# Optional: publish the rebuilt result to the combined PR's writable head.
# Required for PR entries because refs/pull/N/head does not name that branch.
reconstruction_publish = "mine:combined-auth-refactor"
```

It is the combined-PR case from the section above, promoted from something the
manifest works around into something it knows. Excluding the parents keeps them
out of the stack, which is necessary but is only half the truth: the other half
is that this entry *contains* them, so when a parent moves, this entry is stale,
and nothing about an exclusion says that or says what to do about it. The pin
records a commit that was built against whatever the parents were that day, and
merging it is merging history that no longer exists anywhere else.

Declaring the relationship lets `build` rebuild the entry instead of merging a
stale pin. For a derived entry it:

1. detaches a second worktree (`.worktrees/derive`) at the pinned base,
2. merges each parent pin in manifest order, `--no-ff`,
3. replays the entry's own commits — its **delta** — on top, one cherry-pick at
   a time,
4. merges *that* into the stack in place of the pin.

Both phases use the same rerere machinery as an ordinary merge, so a conflict
between two parents is recorded and replayed exactly like a conflict between
two entries, and is attributed to the entry that declared them. The entry's
result still records its pin: the pin is what the manifest tracks and what
`update` moves. The reconstruction is recorded beside it.

`reconstruction_publish = "REMOTE:BRANCH"` opts a derived entry into updating
that writable branch after a *complete* successful build. This supports a
combined PR whose source is `pr = N`: the PR ref is fetch-only, so the manifest
must name its writable fork branch explicitly. Publication uses a
force-with-lease, so a concurrent review update aborts the build's publication
instead of being overwritten. `build --locked` is read-only and always skips
the requested publication.

Parents are neither entries nor exclusions. They are carried — inside the entry
that merged them — so the manifest refuses to also carry one as an entry, and
refuses to also exclude one: the declaration already keeps discovery away, and
says why more precisely than a reason string could. Two entries may share a
parent; each reconstruction is standalone, and identical content merges
cleanly.

#### The anchor, and why not `git cherry`

Everything above depends on knowing which commits are the entry's own. The
obvious answers are wrong. Both `git cherry` (patch-id equivalence) and
`rev-list C ^A ^B` (reachability) define "C's own commits" against the parents'
**live tips** — which is exactly the comparison that a rebased parent breaks.
After the rebase, the copies of A's commits inside C's history match nothing
reachable from the new A, so they are classified as C's own work and replayed
on top of the new A: the content the rebase replaced comes back, silently, in
the assembled tree.

So the lock records an **anchor** instead: a commit inside C's own history,
after which C's own commits start. The delta is then
`rev-list --reverse --no-merges <pin> ^<anchor>`, which is exact and cannot be
perturbed by anything happening on a parent branch.

The anchor is established whenever the entry's pin is — by `update`, or by the
entry's first build — and by nothing else, for the same reason `build` never
moves a pin: what a build replays must not depend on when it ran. Three rules,
in order, and which one fired is always printed, because a wrong boundary
duplicates or drops work and the operator is the only one who can see that
before it lands:

1. **The last reconstruction's parent merge is an ancestor of the new pin.**
   The operator pushed the reconstructed tip and the PR then gained commits —
   review fixes, typically. That merge *is* the boundary, and everything above
   it is the entry's own work, including the delta that was replayed to build
   it. Nothing is replayed twice.
2. **The recorded anchor is still an ancestor of the new pin.** The entry grew
   normally; the old boundary still marks the same place. Keep it.
3. **Detect.** Walk the pin's first-parent chain to the first commit that is a
   merge, or that the base or a parent already contains. Above it is the
   entry's own work. The tip itself qualifying means the entry is a pure merge
   of its parents and its delta is empty.

If the walk reaches the root without a hit, the build stops and says so: the
branch does not look like a merge of its parents, and there is no boundary to
find.

Known limitations, by design:

- **Parents must be merged in, not cherry-picked.** The anchor is a merge
  commit or a contained commit; a branch that rebases its parents' work onto
  itself keeps no record of where they end, and detection fails rather than
  guessing.
- **Detection stops at the first merge, whatever it merged.** A branch that
  merges its parents, adds work, and *then* merges upstream (or a third topic)
  anchors on that later merge, so the work below it is not in the delta and is
  not replayed. Rule 3 is a heuristic over a shape it cannot verify, which is
  why the anchor is printed every time it is chosen and shown by `status`:
  check the delta count against what the branch actually added. Rules 1 and 2
  mean the boundary only has to be right once — after that it is a recorded
  fact, and correcting a bad one is an edit to `pins.anchors` in the lock,
  which `build` then consumes like any other pin.
- **The delta may not contain merges.** Replaying is a sequence of
  cherry-picks, which cannot reproduce a merge. Merge commits above the anchor
  are skipped, so a derived entry whose own work merges something is outside
  what this reconstructs — carry it as an ordinary entry, or declare that
  something as a parent too.
- **Reproducibility is in trees, not commits.** Reconstruction generates fresh
  commits with real timestamps, so the same inputs give the same trees and
  different OIDs — the same bargain the assembled branch already makes, and the
  lock's tree hash still catches anything replayed wrongly.

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
  and for a fixup that no longer applies, and `fork-assembler fixup ENTRY PATH
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

### Base conflicts are refused, not resolved

A resolution is only ever recorded for a conflict *between* things this
repository composes. A topic that cannot merge with the base **on its own** is
not composing badly — it is simply out of date with upstream, and that is a fact
about the topic, not about the assembly.

So the moment a merge conflicts, the build asks `git merge-tree --write-tree`
whether the topic and the base alone conflict. If they do, it aborts the merge,
clears its state, and exits non-zero with the topic, the base, the conflicted
paths, and the rebase to run. This happens *before* rerere is consulted, so a
pair recorded when the conflict was still cross-topic cannot start silently
absorbing upstream's changes once the base moves onto the same lines.

Refusing is the stronger choice over stopping for resolution, for three reasons:

- **The fix has an owner elsewhere.** The topic's own author and reviewers
  cannot see a conflict resolved in a downstream assembly. If the topic is a PR,
  it is blocked for its maintainer too — rebasing it is work that needed doing.
- **The resolution would not stay resolved.** A cross-topic pair is keyed on
  hunks that only these topics produce. A base conflict recurs, differently,
  with every upstream commit that touches those lines.
- **It keeps topics upstreamable**, which is the core invariant. A topic
  carried only by a resolution in this repository has quietly stopped being a
  minimal diff against upstream.

`status` reports the same condition as `CONFLICTS WITH BASE`, on entries and on
derived entries' parents, so it is visible before a build has to refuse.

Derived entries are checked one parent at a time, as each is merged onto the
base during reconstruction. The entry's own merge into the stack is not checked
against the base, because what it merges is the reconstruction, which already
contains it.

### Manifest and lock

`manifest.toml` is **intent** (which refs are tracked, in what order).
`manifest.lock.json` is **fact** (which OIDs the last build used, the assembled
commit, and its tree hash). Cargo/Bundler semantics.

Concretely the lock has two parts: `pins` (base OID plus per-entry-name OID —
patch entries pin the file's blob hash; derived entries additionally pin each
parent and their anchor; moved only by `update` or an entry's first build) and
`build` (the last completed build: `commit`,
`pre_provenance_commit`, `tree` — the pre-provenance content tree, the
reproducibility invariant — `built_tree`, per-entry results with
merged/absorbed/empty/applied status plus conflict/resolution info — and, for a
derived entry, the two commits its reconstruction produced — and a snapshot of
the manifest entries used to detect prefix-extension, each carrying its entry's
fixup blob hash, parent pins, and anchor when it has them, so that editing a
fixup, repinning a parent, or re-anchoring invalidates the suffix from that
entry exactly as repinning the entry would). Branch entries may
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
parents = [{ pr = 3970 }, { branch = "mine:custom-snooze-base" }]
                                # 3984 merges these and builds on them: `build`
                                # re-merges them onto the base and replays
                                # 3984's own commits on top. Declaring a parent
                                # keeps discovery away from it, so it must NOT
                                # also be carried or excluded.

[[entry]]
patch = "patches/site-local-branding.patch"  # standalone, at its own position

[[exclude]]
pr = 3971                       # deliberately not carried; discovery skips it
reason = "abandoned by its author; the fix landed in 3984"
```

### Append machinery / incremental builds

Ordering invariant, designed in deliberately:

> A build whose manifest is a **prefix-extension** of the lock resumes from the
> last assembled commit and processes only the new entries.

Appending never re-touches or re-resolves earlier entries, so "throw stuff on
top" is cheap:

```
fork-assembler add mine:some-branch
fork-assembler add --pr 4102
fork-assembler add --patch fix-thing.patch
fork-assembler build          # incremental: merges only the tail
```

Reordering or removing an entry invalidates the suffix from that point and
triggers a rebuild from there — detectable via the lock, never surprising.

## Verbs (v1)

- `build` — assemble the stack from the lock's pinned OIDs (fetching objects
  as needed), auto-resolving conflicts from the tracked rerere pairs. Entries
  not yet in the lock are pinned from live refs on their first build. Derived
  entries are reconstructed first, in a second worktree, and the reconstruction
  is merged in place of the pin. Stops in the build worktree — or, during a
  reconstruction, in the derive worktree, which it names — on an unrecognized
  conflict. After all entries succeed, configured derived entries publish their
  reconstruction to their explicit writable branches. `--locked` additionally
  refuses to touch the network, pin anything new, or publish a reconstruction.
- `update [ENTRY...]` — the pin bump: repin the base and all entries (or only
  the named ones) to their live remote heads, including each derived entry's
  parents, after which it re-establishes that entry's anchor and reports which
  rule chose it. `build` never moves existing pins; `update` is the only verb
  that does. After a batch bump, `build`
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
  `patches/`, `flake.nix` (consuming `fork-assembler.lib.mkMaintenanceShell`),
  `.envrc`, justfile. `--upstream URL` fills in the base remote; `--submodule`
  additionally adds the upstream as a git submodule at `upstream/` and marks
  it in the manifest as the base-object source (an optional sourcing strategy,
  never a requirement). The same layout is available as a nix flake template
  (`nix flake init -t github:colonelpanic8/fork-assembler`).
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

A "stack repo" that uses fork-assembler contains only data:

```
manifest.toml
manifest.lock.json
resolutions/
patches/
```

Publish/install tails (pushing the assembled branch, tagging, Nix repinning,
system activation) are site-specific and live in the consuming repo (justfile,
scripts) — outside fork-assembler.

### Nix integration

Beyond `packages` and `devShells`, the fork-assembler flake exports:

- `overlays.default` — adds `pkgs.fork-assembler`.
- `lib.mkMaintenanceShell { pkgs, extraPackages ? [] }` — dev shell for
  maintenance repos: the compiled fork-assembler binary plus `git`, `gh`, and
  `just`. Consuming repos pair it with direnv (`use flake` in `.envrc`).
- `templates.default` — the maintenance-repo scaffold, kept in
  `templates/maintenance/` and shared verbatim with `fork-assembler init` via
  `include_str!`.
- `lib.agentGuide` — the authoritative agent operating guide as text.
  Maintenance flakes re-export it from their pinned `fork-assembler` input, letting
  a stable local discovery stub load version-matched instructions with
  `nix eval` rather than vendoring a copy.

## Agent-first operation

fork-assembler assumes its day-to-day operators are coding agents as much as
humans. Consequences:

- The maintenance template ships `AGENTS.md` (model + invariants +
  operations), a `CLAUDE.md` pointer, and a project skill covering status,
  appends, and the conflict-resolution loop. The skill lives canonically at
  `.agents/skills/fork-assembler/` in the open Agent Skills format
  (agentskills.io); `.claude/skills/` and `.codex/skills/` are relative
  symlinks into it, so Claude Code and Codex share one entry point and other
  agents find it via AGENTS.md. That local skill is a small, stable discovery
  stub. It evaluates `lib.forkFoldAgentGuide`, re-exported directly from the
  repository's pinned `fork-assembler` input, so the substantive instructions
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
  parent manifest. Derived entries are the one case of this that shipped: a
  combined PR *is* a nested assembly, but one somebody else already maintains
  and publishes, so fork-assembler only has to know its inputs and rebuild it, not
  own its order, resolutions, or lock. General groups are still deferred; add
  them when a consumer has a repeatedly-conflicting subsystem cluster that no
  upstream branch already combines.
- **Freeze/vendor** — hermetic mode: vendored bundles (base + topics) so a
  clone reproduces with no network. The t3code workflow may want this back;
  the default is live refs + lock pins.
- Anything resembling patch algebra. fork-assembler pins order and records outcomes;
  it does not try to make patches commute.
