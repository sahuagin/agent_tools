# scripts/ — vendored host tooling (source of truth for ~/.local/bin)

Hand-written shell tooling that runs from `~/.local/bin`. This repo is the source
of truth; deploy by symlinking `~/.local/bin/<tool>` → the copy here (the pattern
`agent` / `mu` / `t4c` already use). Nothing executed moves — the symlink just
points the on-PATH name at the versioned source.

**Not vendored here:** externally-installed tools — uv/npm/pip packages
(`sparklines`), built binaries (`beads`, `beadsd`, `bw`), etc. Those install or
build+deploy from their own packages/repos; checking them in is wrong.

**`hooks/` deploys to `~/.claude/hooks/`,** not `~/.local/bin` — these are Claude
Code hooks, wired by absolute path in `~/.claude/settings.json`. Symlink them
*relatively* (`../../src/public_github/agent_tools/scripts/hooks/<hook>`).
`~/.claude` is itself a repo (`dotclaude`) checked out under more than one home,
and that relative path resolves in each of them; an absolute target would dangle
in every home but the one it was written for (at-cj2).

## Tools

- **sprint-start** — claim a bead + enter a unique per-bead jj workspace,
  atomically (correct-by-construction isolation: atomic beads claim + a sibling jj
  workspace, so two sessions can't collide). Pairs with `sprint-end`.
- **deploy-local** — bring THIS host's deployed tooling up to trunk and report
  what it did (at-g0s). Merged is not deployed: scripts symlink to a working
  copy, plain binaries need a build, `emu` tools self-build from their checkout,
  and services need a restart. `--status` answers "what is this host running?";
  `--dry-run` shows the plan. Refuses to advance a checkout with work in it, and
  never restarts a service.
- **sprint-end** — release a sprint's bead and tear down its jj workspace
  (`--close` closes the bead instead of unclaiming; refuses while the workspace is
  dirty unless `--force`). Run bare from *inside* a workspace it refuses with a
  redirect to the arg form (`cd <root> && sprint-end <token>`), because removing
  the caller's cwd would strand the shell on getcwd — a child process can't cd its
  parent back out.
- **sprint-lib.sh** — shared library for sprint-start/end (beadsd endpoint + actor
  resolution, trunk revision). Sourced by both as `$HOME/.local/lib/sprint-lib.sh`,
  so it deploys to `~/.local/lib/` (not `bin/`).
- **sprint-funcs.sh** — bash shell helpers. Defines `sprint-end` as a shell
  *function* (shadowing the binary) that hops the shell to the repo root before the
  binary tears the workspace down, so bare `sprint-end` from inside a workspace Just
  Works with no strand. Deploys to `~/.local/lib/` (not on PATH). It's for the
  *consumers* of sprint-start/end: load it by sourcing from `~/.bashrc` (interactive
  bash) or by giving a consumer script a `#!/usr/bin/env bash` shebang and
  `. ~/.bashrc` near the top.
- **hooks/dialogue-rewake.sh** — Claude Code `Stop` hook (asyncRewake). Long-polls
  the mu-dialogue mailbox while a session is idle and wakes the model when a peer
  writes. Deploys to `~/.claude/hooks/dialogue-rewake.sh` rather than `~/.local/bin`
  (it is a hook, not a PATH tool), and is wired up in `~/.claude/settings.json`
  under `hooks.Stop`.

  It declines to arm when `CLAUDE_CODE_ENTRYPOINT` starts with `sdk-`, which is
  what `claude -p` and the SDKs report. A one-shot run exits when its turn ends,
  so an idle watch armed for it has nobody to wake and the caller waits on it —
  that was the nested-claude hang, and it cost up to the 30-minute cap. The check
  is a deny-list on purpose: an unrecognised or unset entrypoint still arms,
  because a wasted background poll is cheaper than silently disabling inter-agent
  messaging for a real user.

  `DIALOGUE_REWAKE_FORCE=1` arms regardless of entrypoint, and
  `DIALOGUE_REWAKE_DEBUG=<file>` records the arm/skip decision. Both exist for
  testing this specific behaviour.

- **hooks/dialogue-session-start.sh** — `SessionStart` half of the same dialogue
  mechanism: registers the session as a peer and seeds the rewake watermark to
  session-start time. Its watermark path and epoch-ms form must match
  `dialogue-rewake.sh` exactly, which is why the two are vendored together.
- **hooks/jj-colocated-git-nudge.sh** — `PreToolUse(Bash)` guard: denies mutating
  raw `git` in a jj repo, and raw `jj git push` to a github remote, steering to
  `sprint-start` / `bot-jj` — both of which live here.
- **hooks/kx-recall.sh** — `UserPromptSubmit` hook; runs a semantic `agent memory`
  recall on the prompt and injects only above-threshold hits.
- **hooks/search-scope-guard.py** — `PreToolUse(Bash|Grep|Glob)` guard: denies
  recursive find/fd/grep/rg rooted at `/`, `/home`, a bare home dir, or another
  root-level dir, steering to a scoped path; otherwise injects `find -x` /
  `fd --one-file-system` (ported from the retired bash-guard V2.1).
- **jj-orphan-audit** — categorize jj loose heads vs a base revision; a recovery
  aid referenced by the `jj-runbook` skill.
