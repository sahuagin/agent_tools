#!/usr/bin/env python3
"""
PreToolUse guard: DENY filesystem-wide recursive searches (at-search-scope-guard-yxr).

Operator rule: "filesystem wide is never the right answer for any question."
A recursive find/fd/grep -r/rg rooted at /, /home, or a bare home directory
sweeps whole datasets on a slow FS (one such grep ran ~7h until killed by
hand, 2026-08-12) and always means the caller hasn't located the target yet.
Resolve the container first (code_sources, t4c, ls), then search inside it.

Covers Bash commands and the Grep/Glob tools' `path` param. Two layers,
ported from the retired bash-guard V2.1 (source of truth:
.172:~tcovert/src/claude-personal, host/home/tcovert/.local/bin/bash-guard):

  1. Broad-root recursive search -> DENY. A confinement flag can't fix
     these — the whole jail is one dataset, so a home-rooted crawl never
     crosses a mount point yet still sweeps everything.
  2. Otherwise, unconfined find/fd -> REWRITE, injecting `find -x` /
     `fd --one-file-system` (jail find is bfs, which takes BSD-style -x
     pre-path). Injected rather than suggested because the model never
     adds the flag consistently on its own (operator, 2026-08-12).
     NOTE: -x is fd's --exec, so it does NOT count as confinement for fd.

DETERRENCE, NOT A BOUNDARY (a script/cron wrapping the same search bypasses
it) — the point is to kill the reflex. Fail-OPEN on any error; deny ONLY on
a confident match.
"""
import json
import os
import re
import shlex
import sys

# Roots at or below which a recursive search is "filesystem wide".
# /home/<user> matches exactly (depth 2); deeper paths are fine.
BROAD_EXACT = {"/", "/home", "/usr", "/var", "/etc", "/opt", "/compat", "/tmp"}
HOME_DIR = re.compile(r"^/home/[^/]+/?$")

DENY = (
    "search-scope-guard: recursive search rooted at {root} is filesystem-wide "
    "— never the right answer. Locate the container first (code-index "
    "code_sources for repos, t4c find / ls for directories, agent memory "
    "recall for prior work), then search that path, e.g. "
    "`rg <pattern> <repo-dir> -g '!target'`."
)


def allow():
    sys.exit(0)


def deny(root):
    print(json.dumps({"hookSpecificOutput": {
        "hookEventName": "PreToolUse", "permissionDecision": "deny",
        "permissionDecisionReason": DENY.replace("{root}", root)}}))
    sys.exit(0)


def rewrite(command, flag):
    print(json.dumps({
        "systemMessage": f"search-scope-guard: added {flag} (one-filesystem "
                         "confinement). Pass it yourself to silence this.",
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "updatedInput": {"command": command}}}))
    sys.exit(0)


# command-position match, same shape as bash-guard's: start or after |;&(
FIND_CMD = re.compile(r"((^|[|;&(])\s*)find(\s)")
FD_CMD = re.compile(r"((^|[|;&(])\s*)fd(\s)")


def inject_confinement(command):
    """bash-guard parity: confine unconfined find/fd to one filesystem."""
    new = command
    if FD_CMD.search(new) and not re.search(
            r"(\s--one-file-system|\s--mount|\s--xdev|\s--max-depth|\s-d\s)", new):
        new = FD_CMD.sub(r"\1fd --one-file-system\3", new)
        flag = "--one-file-system"
    if FIND_CMD.search(new) and not re.search(r"(\s-x\s|\s-xdev|\s-maxdepth\s)", new):
        new = FIND_CMD.sub(r"\1find -x\3", new)
        flag = "-x"
    if new != command:
        rewrite(new, flag)


def is_broad(path, home):
    p = os.path.expanduser(path)
    for var in ("$HOME", "${HOME}"):
        if p == var or p.startswith(var + "/"):
            p = home + p[len(var):]
            break
    p = p.rstrip("/") or "/"
    return p in BROAD_EXACT or bool(HOME_DIR.match(p + "/"))


def segments(command):
    """Split a shell command into pipeline/list segments (best effort)."""
    try:
        toks = shlex.split(command, posix=True)
    except ValueError:
        return []
    segs, cur = [], []
    for t in toks:
        if t in ("|", "||", "&&", ";", "&"):
            if cur:
                segs.append(cur)
            cur = []
        else:
            cur.append(t)
    if cur:
        segs.append(cur)
    return segs


def bare_args(args):
    """Non-flag arguments (skip flags; can't know which flags take values,
    so a flag's value may appear here — acceptable for a confident-match
    guard: option values that look like broad roots are worth flagging
    anyway, and anything else is ignored)."""
    return [a for a in args if not a.startswith("-")]


def check_command(command, cwd, home):
    for seg in segments(command):
        # skip env-var prefixes (FOO=bar cmd ...)
        while seg and "=" in seg[0] and not seg[0].startswith("-"):
            seg = seg[1:]
        if not seg:
            continue
        tool = os.path.basename(seg[0])
        args = seg[1:]
        if tool == "find":
            # skip BSD/bfs pre-path flags (-f takes a value), then
            # paths = args before the first expression token
            while args and args[0] in ("-H", "-L", "-P", "-E", "-X",
                                       "-d", "-s", "-x"):
                args = args[1:]
            while len(args) >= 2 and args[0] == "-f":
                args = args[2:]
            paths = []
            for a in args:
                if a.startswith("-") or a in ("(", "!"):
                    break
                paths.append(a)
            for p in paths or [cwd]:
                if is_broad(p, home):
                    deny(p)
        elif tool in ("fd", "fdfind", "rg", "ripgrep"):
            # <tool> [flags] pattern [path...] — recursive by default.
            # A single bare arg is a pattern searching cwd; two or more
            # means explicit path(s) (a flag value miscounted as a path
            # just skips the cwd check — fail-open).
            bare = bare_args(args)
            hit = [a for a in bare if is_broad(a, home)]
            if hit:
                deny(hit[0])
            if len(bare) <= 1 and is_broad(cwd, home):
                deny(cwd)
        elif tool in ("grep", "egrep", "fgrep"):
            recursive = any(
                a in ("-r", "-R", "--recursive", "--dereference-recursive")
                or (re.match(r"^-[a-zA-Z]+$", a) and ("r" in a[1:] or "R" in a[1:]))
                for a in args)
            if not recursive:
                continue
            bare = bare_args(args)
            hit = [a for a in bare if is_broad(a, home)]
            if hit:
                deny(hit[0])
            if len(bare) <= 1 and is_broad(cwd, home):
                deny(cwd)


def main():
    data = json.load(sys.stdin)
    cwd = data.get("cwd") or os.getcwd()
    home = os.path.expanduser("~")
    tool_name = data.get("tool_name", "")
    ti = data.get("tool_input") or {}
    if tool_name == "Bash":
        command = ti.get("command") or ""
        if command:
            check_command(command, cwd, home)   # deny wins over rewrite
            inject_confinement(command)
    elif tool_name in ("Grep", "Glob"):
        path = ti.get("path") or cwd
        if is_broad(path, home):
            deny(path)
    allow()


if __name__ == "__main__":
    try:
        main()
    except SystemExit:
        raise
    except Exception:
        sys.exit(0)
