# fork-assembler

Maintain a build recipe for a stack of live fork branches: an upstream base
plus an ordered set of topic branches (yours or other people's PRs), assembled
into a single branch with tracked, replayable conflict resolutions.

Topics stay clean, minimal diffs against upstream — still viable merge
targets — while the assembled branch is disposable compiled output. See
[DESIGN.md](DESIGN.md) for the model.

```
fork-assembler add mine:custom-snooze
fork-assembler add --pr 3984
fork-assembler exclude --pr 3970 --reason "superseded by 3984"
fork-assembler build
```

Excluding is how the manifest says no. Deleting an entry records nothing:
`add --prs-from USER` re-appends every open PR it finds, so a target that must
stay out — a PR superseded by a combined one that already contains it, say —
needs its refusal written down where a sweep will see it.

Resolutions are only ever recorded for conflicts *between* carried topics. A
topic that cannot merge with the base on its own is out of date with upstream,
and `build` refuses it instead of offering to resolve it — the fix is a rebase
on the topic branch, pushed where its author and reviewers can see it.

The Rust CLI implements build, update, append, repair, status, and prune
workflows. The `t3code-assembly` repository is the reference real-world
consumer.

Maintenance repositories check in only a stable agent-skill discovery stub.
The full guide is exported by—and loaded directly from—the `fork-assembler`
revision pinned in their `flake.lock`, so changing that input changes the
instructions without a synchronization step.
