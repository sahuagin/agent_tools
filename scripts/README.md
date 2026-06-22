# scripts/ — vendored host tooling (source of truth for ~/.local/bin)

Hand-written shell tooling that runs from `~/.local/bin`. This repo is the source
of truth; deploy by symlinking `~/.local/bin/<tool>` → the copy here (the pattern
`agent` / `mu` / `t4c` already use). Nothing executed moves — the symlink just
points the on-PATH name at the versioned source.

**Not vendored here:** externally-installed tools — uv/npm/pip packages
(`sparklines`), built binaries (`beads`, `beadsd`, `bw`), etc. Those install or
build+deploy from their own packages/repos; checking them in is wrong.

## Tools

- **sprint-start** — claim a bead + enter a unique per-bead jj workspace,
  atomically (correct-by-construction isolation: atomic beads claim + a sibling jj
  workspace, so two sessions can't collide). Pairs with `sprint-end`.
- **sprint-end** — release a sprint's bead and tear down its jj workspace
  (`--close` closes the bead instead of unclaiming; refuses while the workspace is
  dirty unless `--force`).
- **sprint-lib.sh** — shared library for sprint-start/end (beadsd endpoint + actor
  resolution, trunk revision). Sourced by both as `$HOME/.local/lib/sprint-lib.sh`,
  so it deploys to `~/.local/lib/` (not `bin/`).
- **jj-orphan-audit** — categorize jj loose heads vs a base revision; a recovery
  aid referenced by the `jj-runbook` skill.
