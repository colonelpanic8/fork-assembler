# fork-fold

Maintain a build recipe for a stack of live fork branches: an upstream base
plus an ordered set of topic branches (yours or other people's PRs), assembled
into a single branch with tracked, replayable conflict resolutions.

Topics stay clean, minimal diffs against upstream — still viable merge
targets — while the assembled branch is disposable compiled output. See
[DESIGN.md](DESIGN.md) for the model.

```
fork-fold add mine:custom-snooze
fork-fold add --pr 3984
fork-fold exclude --pr 3970 --reason "superseded by 3984"
fork-fold build
```

Excluding is how the manifest says no. Deleting an entry records nothing:
`add --prs-from USER` re-appends every open PR it finds, so a target that must
stay out — a PR superseded by a combined one that already contains it, say —
needs its refusal written down where a sweep will see it.

The Rust CLI implements build, update, append, repair, status, and prune
workflows. The `t3code-assembly` repository is the reference real-world
consumer.

Maintenance repositories check in only a stable agent-skill discovery stub.
The full guide is exported by—and loaded directly from—the `fork-fold`
revision pinned in their `flake.lock`, so changing that input changes the
instructions without a synchronization step.
