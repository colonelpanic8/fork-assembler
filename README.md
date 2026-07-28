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
fork-fold build
```

Status: design phase. The reference prototype for these semantics is the
`t3code-assembly` repository's Python tooling.
