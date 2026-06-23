# sprint-lib.sh — shared resolvers for sprint-start / sprint-end.
#
# The job: from anywhere inside a colocated git+jj repo (backing workspace OR a
# secondary jj workspace), resolve the BACKING repo root and its canonical
# .beads DB — so bead claims always hit ONE database regardless of which
# workspace the caller stands in. This is the load-bearing fix for the
# divergent-workspace-DB failure (br resolves its DB by cwd-walk and a jj
# workspace is a SIBLING of the backing repo, so a naive `br` finds a divergent
# local DB instead of the canonical one).
#
# VERIFIED 2026-05-31: in the backing/default workspace `.jj/repo` is a
# DIRECTORY; in a secondary workspace it is a FILE holding a RELATIVE path to
# <backing>/.jj/repo. Relative => relocation-proof. Encapsulated here so a jj
# layout change is a one-line fix.

# backing_root [start-dir] -> absolute path of the backing repo root, or empty.
backing_root() {
    start="${1:-$PWD}"
    root=$(cd "$start" 2>/dev/null && jj root 2>/dev/null) || return 1
    p="$root/.jj/repo"
    if [ -f "$p" ]; then
        # secondary workspace: pointer file -> <backing>/.jj/repo
        t=$(cat "$p")
        case "$t" in
            /*) : ;;
            *) t="$root/.jj/$t" ;;   # resolve relative to the .jj/ dir
        esac
        ( cd "$(dirname "$(dirname "$t")")" 2>/dev/null && pwd )
    else
        printf '%s\n' "$root"
    fi
}

# canonical_db [start-dir] -> path to the backing repo's canonical beads DB.
canonical_db() {
    r=$(backing_root "$1") || return 1
    [ -n "$r" ] || return 1
    printf '%s/.beads/beads.db\n' "$r"
}

# beads_endpoint <repo> -> the beadsd MCP url for that project, or empty.
# Projects centralized into beadsd route claims/releases through the single-
# writer service (so cross-host/cross-workspace claims can't diverge); projects
# without an entry keep using the local .beads DB. The map is one `repo=url` per
# line in ~/.config/beads/remotes.env.
beads_endpoint() {
    f="${XDG_CONFIG_HOME:-$HOME/.config}/beads/remotes.env"
    [ -f "$f" ] || return 0
    sed -n "s/^$1=//p" "$f" | head -n1
}

# session_actor -> stable, readable per-session id used as the claim's assignee.
# Two distinct Claude sessions get distinct actors (so a claim by one refuses
# the other); the SAME session recomputes the SAME actor (so re-runs are idempotent).
# This is the FALLBACK identity when the operator didn't pass `--as <name>`.
session_actor() {
    sid="${CLAUDE_CODE_SESSION_ID:-}"
    prof="${CLAUDE_PROFILE:-personal}"
    if [ -n "$sid" ]; then
        printf 'cc-%s-%s\n' "$prof" "$(printf '%s' "$sid" | cut -c1-8)"
    else
        printf 'cc-%s-nosess-%s\n' "$prof" "$$"
    fi
}

# A sprint records the exact actor it claimed with (and whether that actor is a
# registered etcd session name) in a sidecar, so sprint-end can RELEASE with the
# same actor it CLAIMED with — otherwise the ownership check ("is this bead held
# by me?") would mismatch and refuse to release a `--as <name>`-claimed bead.
#
# The sidecar lives OUTSIDE the workspace, in a state dir keyed by the
# workspace's absolute path. Putting it inside would make the working tree dirty
# and trip sprint-end's own commit-before-release guard (learned the hard way).
sprint_state_dir() {
    printf '%s/sprint\n' "${XDG_STATE_HOME:-$HOME/.local/state}"
}

# sprint_sidecar <workspace-dir> -> the state file path for that workspace.
sprint_sidecar() {
    printf '%s/%s.env\n' "$(sprint_state_dir)" "$(printf '%s' "$1" | sed 's|[^A-Za-z0-9._-]|_|g')"
}

# sprint_write_actor <workspace-dir> <actor> <registered:0|1>
sprint_write_actor() {
    d=$(sprint_state_dir); mkdir -p "$d"
    printf 'actor=%s\nregistered=%s\n' "$2" "$3" > "$(sprint_sidecar "$1")"
}

# sprint_actor [workspace-dir] -> the actor this sprint claimed with. Prefer the
# sidecar (so release matches claim); fall back to the auto session actor.
sprint_actor() {
    ws="${1:-$PWD}"; f=$(sprint_sidecar "$ws")
    if [ -f "$f" ]; then
        sed -n 's/^actor=//p' "$f" | head -n1
    else
        session_actor
    fi
}

# sprint_registered [workspace-dir] -> 1 if this sprint's actor is a registered
# etcd session name (so sprint-end should `agent-slot unregister` it), else 0.
sprint_registered() {
    f=$(sprint_sidecar "${1:-$PWD}")
    [ -f "$f" ] || { printf '0\n'; return; }
    r=$(sed -n 's/^registered=//p' "$f" | head -n1)
    printf '%s\n' "${r:-0}"
}

# sprint_clear_actor <workspace-dir> -> remove the sidecar (sprint-end teardown).
sprint_clear_actor() {
    rm -f "$(sprint_sidecar "$1")" 2>/dev/null || true
}

# trunk_rev [repo-root] -> a revset for the repo's trunk (main/master). jj's
# trunk() resolves the conventional default branch; fall back to main.
trunk_rev() {
    if jj log --no-graph -r 'trunk()' -T '""' >/dev/null 2>&1; then
        printf 'trunk()\n'
    else
        printf 'main\n'
    fi
}
