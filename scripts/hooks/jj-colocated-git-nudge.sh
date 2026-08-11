#!/usr/bin/env python3
"""
PreToolUse(Bash) guard for cc in jj-colocated repos. TWO cc-shaping jobs only:

  1) Mutating raw `git` in a jj repo  -> DENY, steer to jj / sprint-start.
  2) Raw `jj git push` to a GITHUB remote -> DENY, steer to `bot-jj git push`
     (the sanctioned wrapper; its internal `jj git push` is a child process,
     not a cc Bash command, so it isn't caught here). Non-github remotes
     (e.g. forgejo dotclaude/dotmu) have no wrapper — bot-jj hard-exits on
     them ("App identity does not apply") — so raw push is the sanctioned
     path there and is allowed (operator-confirmed 2026-07-10).

History: leak prevention was briefly hand-rolled here + in a bot-jj patch,
removed in favor of a planned general jj-hooks leak gate (`jj-hp push` ->
prek -> scan-hardcoded; design: bead mu-8puo.2) that this hook described as
adopted. Terrain check 2026-07-10: that gate was never built (no jj-hp/prek/
scan-hardcoded on any machine), so pushes steer to bot-jj until it exists.
Raw-git-deny started as a nudge (postmortem-2026-06-22), failed twice
advisory -> now DENY.

DETERRENCE, NOT A BOUNDARY (python/child-cc bypass it) — the point is to kill
the reflex. Fail-OPEN on any error; deny ONLY on a confident match.
"""
import sys, os, json, re, subprocess

GIT_INVOCATION = re.compile(r'(?:^|[;&|()\n])\s*git(?=\s|$)')
RAW_JJ_PUSH = re.compile(r'(?:^|[;&|()\n])\s*jj\s+git\s+push\b')  # raw; not `bot-jj git push`
# Raw `gh pr create|merge` — not `bot-gh` (its 'gh' follows '-', no separator).
RAW_GH_PR = re.compile(r'(?:^|[;&|()\n])\s*gh\s+pr\s+(create|merge)\b')

GIT_ONLY = {"worktree", "push", "am", "apply", "filter-repo", "filter-branch"}
MUTATING_JJ = {
    "commit", "commit-tree", "rebase", "merge", "reset", "checkout", "switch",
    "restore", "clean", "stash", "cherry-pick", "revert", "add", "rm", "mv",
    "init", "clone", "pull", "branch", "tag", "update-ref", "update-index",
    "gc", "prune", "sparse-checkout", "submodule", "notes", "replace", "mergetool",
}

JJ_ALT = (
    "  - isolated work -> `sprint-start <bead-id>` (or `--no-bead <label>`); tear down `sprint-end`.\n"
    "  - commit -> `jj describe -m ... && jj new`;  branch -> `jj bookmark`.\n"
    "  - checkout/switch -> `jj new <rev>` / `jj edit <rev>`;  reset/restore -> `jj restore` / `jj abandon`.\n"
    "  - if jj state looks wrong -> `jj op log` then `jj op restore`."
)
DENY_JJ = ("BLOCKED: raw `git {sub}` in a jj-colocated repo. jj does this directly.\n" + JJ_ALT)
DENY_GIT_ONLY = ("BLOCKED: raw `git {sub}` in a jj-colocated repo. This git-only op is a deliberate "
                 "HUMAN/operator step per the jj-runbook, not an agent one.\n" + JJ_ALT)
DENY_PUSH = ("BLOCKED: raw `jj git push`. Push via `bot-jj git push` (the sanctioned "
             "App-identity wrapper; same flags after the subcommand). For a github repo "
             "the App is NOT installed on (bot-jj exits 'App identity does not apply'; "
             "e.g. splinedataco/*), acknowledge the manual push explicitly: "
             "`JJ_PUSH_NO_APP=1 jj git push ...` (operator-approved 2026-07-13). A "
             "general pre-push leak gate (`jj-hp push`, design: bead mu-8puo.2) was "
             "planned but never built — do not go looking for it.")
DENY_GH_PR = ("BLOCKED: raw `gh pr {sub}` on a sahuagin/* repo. Author PRs via "
              "`bot-gh pr create` (the App identity, so the operator can APPROVE instead "
              "of admin-overriding his own PR), and NEVER self-merge — merging is the "
              "operator's. Scar: mu #531/#532/#537 were opened raw despite three feedback "
              "memories, forcing force-merges. Repos OUTSIDE sahuagin/* (work repos, no "
              "App) are not guarded — raw gh is the sanctioned path there. Override if "
              "truly needed: `GH_PR_NO_APP=1 gh pr {sub} ...`.")


SAHUAGIN_TARGET = re.compile(r'(?:^|\s)-R\s+sahuagin/|github\.com[:/]sahuagin/')


def gh_targets_sahuagin(cmd, dirs):
    """Confirm-only: True when -R names sahuagin/* or a candidate dir's github
    remote is sahuagin/*. Fail-open (False) otherwise — work repos without the
    App must keep raw gh."""
    if SAHUAGIN_TARGET.search(cmd):
        return True
    if re.search(r'(?:^|\s)-R\s+\S', cmd):
        return False  # explicit non-sahuagin target
    for d in dirs:
        r = find_root(d)
        if not r:
            continue
        try:
            if os.path.exists(os.path.join(r, ".git")):
                c = ["git", "-C", r, "config", "--get-regexp", r"^remote\..*\.url$"]
            else:
                c = ["jj", "git", "remote", "list"]
            out = subprocess.run(c, cwd=r, timeout=3, capture_output=True, text=True).stdout
            if re.search(r'github\.com[:/]sahuagin/', out):
                return True
        except Exception:
            pass
    return False


def allow_silently():
    sys.exit(0)


def find_root(start):
    try:
        d = os.path.abspath(start)
    except Exception:
        return None
    while True:
        if os.path.isdir(os.path.join(d, ".jj")) or os.path.exists(os.path.join(d, ".git")):
            return d
        parent = os.path.dirname(d)
        if parent == d:
            return None
        d = parent


def is_jj(d):
    r = find_root(d)
    return bool(r and os.path.isdir(os.path.join(r, ".jj")))


def has_github_remote(d):
    """True only when we can CONFIRM a github.com remote (fail-open otherwise)."""
    r = find_root(d)
    if not (r and os.path.isdir(os.path.join(r, ".jj"))):
        return False
    try:
        # Remote URLs only — a raw config read false-positives on e.g. a
        # noreply.github.com user email.
        if os.path.exists(os.path.join(r, ".git")):
            cmd = ["git", "-C", r, "config", "--get-regexp", r"^remote\..*\.url$"]
        else:
            cmd = ["jj", "git", "remote", "list"]
        out = subprocess.run(cmd, cwd=r, timeout=3,
                             capture_output=True, text=True).stdout
        return "github.com" in out
    except Exception:
        return False


def candidate_dirs(cmd, cwd):
    dirs = [cwd]
    for pat in (r'\bcd\s+([^\s;&|()]+)', r'(?:^|\s)-C\s+([^\s;&|()]+)', r'(?:^|\s)-R\s+([^\s;&|()]+)'):
        for m in re.finditer(pat, cmd):
            p = os.path.expanduser(m.group(1).strip('"\''))
            if not os.path.isabs(p):
                p = os.path.join(cwd, p)
            dirs.append(p)
    return dirs


def git_subcommands(cmd):
    subs = []
    for m in GIT_INVOCATION.finditer(cmd):
        seg = re.split(r'[;&|()\n]', cmd[m.end():], maxsplit=1)[0]
        toks, i, found = seg.split(), 0, ""
        while i < len(toks):
            t = toks[i]
            if t in ("-C", "-c"):
                i += 2; continue
            if t.startswith("-"):
                i += 1; continue
            found = t; break
        subs.append(found)
    return subs


def strip_literals(cmd):
    out, delim = [], None
    for line in cmd.split("\n"):
        if delim is None:
            m = re.search(r"<<-?\s*['\"]?([A-Za-z_]\w*)['\"]?", line)
            if m:
                delim = m.group(1); out.append(line[:m.start()])
            else:
                out.append(line)
        elif line.strip() == delim:
            delim = None
    s = "\n".join(out)
    s = re.sub(r"'[^']*'", " ", s)
    s = re.sub(r'"[^"]*"', " ", s)
    return s


def emit_deny(reason):
    print(json.dumps({"hookSpecificOutput": {
        "hookEventName": "PreToolUse", "permissionDecision": "deny",
        "permissionDecisionReason": reason}}))
    sys.exit(0)


def main():
    try:
        data = json.load(sys.stdin)
    except Exception:
        allow_silently()
    cmd = ((data.get("tool_input") or {}).get("command")) or ""
    cwd = data.get("cwd") or os.getcwd()
    scan = strip_literals(cmd)
    dirs = candidate_dirs(scan, cwd)

    # 0) Raw `gh pr create|merge` -> steer to bot-gh (App authorship so the
    #    operator can approve; self-merge is never the agent's). Same designed
    #    escape shape as the push guard: GH_PR_NO_APP=1 acknowledges a repo
    #    the App is not installed on.
    if "GH_PR_NO_APP=1" not in cmd:
        m = RAW_GH_PR.search(scan)
        if m and gh_targets_sahuagin(cmd, dirs):
            emit_deny(DENY_GH_PR.replace("{sub}", m.group(1)))

    # 1) Raw `jj git push` to a github remote -> steer to `bot-jj git push`.
    #    Non-github remotes: bot-jj can't serve them; raw push is the path.
    #    JJ_PUSH_NO_APP=1 = the acknowledged-manual-push override for github
    #    repos the App is not installed on (deterrence preserved: explicit,
    #    greppable, named in the deny text). NB an env-prefixed command
    #    would slip RAW_JJ_PUSH anyway (no separator before `jj`); this
    #    makes the escape designed rather than accidental.
    if "JJ_PUSH_NO_APP=1" in cmd:
        allow_silently()
    if RAW_JJ_PUSH.search(scan) and any(has_github_remote(d) for d in dirs):
        emit_deny(DENY_PUSH)

    # 2) Raw mutating git in a jj repo.
    if not GIT_INVOCATION.search(scan):
        allow_silently()
    if not any(is_jj(d) for d in dirs):
        allow_silently()
    subs = git_subcommands(scan)
    jj_ops = [s for s in subs if s in MUTATING_JJ]
    git_only = [s for s in subs if s in GIT_ONLY]
    if jj_ops:
        emit_deny(DENY_JJ.replace("{sub}", jj_ops[0]))
    if git_only:
        emit_deny(DENY_GIT_ONLY.replace("{sub}", git_only[0]))
    allow_silently()


if __name__ == "__main__":
    try:
        main()
    except SystemExit:
        raise
    except Exception:
        allow_silently()
