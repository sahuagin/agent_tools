# sprint-funcs.sh — bash shell helpers for the sprint-* workflow.
#
# Defines `sprint-end` as a shell FUNCTION that shadows the PATH binary so it can
# relocate the shell out of a sprint workspace BEFORE the workspace is removed.
# The binary alone (a child process) can't cd its parent shell, so a bare
# `sprint-end` run from inside a workspace would rm the shell's cwd and strand it
# on getcwd. The function hops to the backing repo root first, then runs the
# binary's strand-free arg form. Anything with a <bead-id> arg (or --help) is
# passed straight through to the binary.
#
# This is for the *consumers* of sprint-start/sprint-end (the shells and scripts
# that run them), not just an interactive convenience. Load it either way:
#   - interactive bash: source from ~/.bashrc
#       . "$HOME/.local/lib/sprint-funcs.sh"
#   - a consumer script: give it a bash shebang and source ~/.bashrc (or this
#     file) near the top —
#       #!/usr/bin/env bash
#       . ~/.bashrc
# Written for bash (also parses clean under sh/zsh); keep edits bash-compatible.

sprint-end() {
    local a positional=0 wants_help=0
    for a in "$@"; do
        case "$a" in
            -h|--help) wants_help=1 ;;
            -*) ;;
            *) positional=1 ;;
        esac
    done
    # A <bead-id> arg → the binary's arg form is already strand-free from
    # anywhere. --help → just show help. Either way, delegate untouched.
    if [ "$positional" -eq 1 ] || [ "$wants_help" -eq 1 ]; then
        command sprint-end "$@"
        return $?
    fi
    # Bare (flags allowed, no <bead-id>): resolve this workspace's backing root +
    # token, hop the shell to that root, then tear down via the strand-free arg
    # form. Resolve the root in an alias-free `sh -c` subshell — sourcing
    # sprint-lib.sh directly in an interactive shell would let a `cd` alias (e.g.
    # zoxide's `z`) hijack backing_root's own internal `cd`. `builtin cd` below
    # likewise bypasses any `cd` alias/function for the actual hop.
    local root ws token
    root=$(command sh -c '. "$HOME/.local/lib/sprint-lib.sh" 2>/dev/null && backing_root' 2>/dev/null) || root=""
    ws=$(command jj root 2>/dev/null) || ws=""
    if [ -n "$root" ] && [ -n "$ws" ] && [ "$ws" != "$root" ]; then
        token=$(basename "$ws")
        token=${token#"$(basename "$root")-"}
        builtin cd "$root" && command sprint-end "$@" "$token"
    else
        # Backing workspace, or not in a jj repo: let the binary explain.
        command sprint-end "$@"
    fi
}
