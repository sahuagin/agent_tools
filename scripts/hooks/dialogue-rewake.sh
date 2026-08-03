#!/bin/sh
# Stop hook (asyncRewake): deterministic inter-agent listener. Runs in the
# background when the session goes idle; long-polls the mu-dialogue mailbox and,
# the moment a peer writes, prints the message(s) and exits 2 — which wakes the
# model (Claude Code appends stdout as a system-reminder). The session then
# handles it, goes idle again, and Stop re-arms this hook. No Monitor, no model
# cooperation required to LISTEN.
#
# Single-instance per session (lockfile) so repeated Stop events don't stack
# pollers. Watermark file persists `since` so each message surfaces once.
#
# Env overrides (testing): DIALOGUE_REWAKE_WM, DIALOGUE_REWAKE_POLL_MS,
# DIALOGUE_REWAKE_MAX, DIALOGUE_REWAKE_SINCE, DIALOGUE_REWAKE_FORCE,
# DIALOGUE_REWAKE_DEBUG.
sid=$(jq -r '.session_id // empty' 2>/dev/null)
[ -n "$sid" ] || sid="$CLAUDE_CODE_SESSION_ID"
[ -n "$sid" ] || exit 0

# ── Arm only for sessions a human will come back to (at-jjo) ────────────────
#
# A one-shot invocation (`claude -p`, or any SDK-driven run) exits when its turn
# finishes, so an idle watch armed for it has nobody to wake — and worse, the
# caller waits on it. That is the nested-claude "hang": ktrace showed the process
# was not blocked on a syscall, it was executing this hook's poll loop, up to the
# 30-minute cap below. `claude --bare` avoids it only because --bare skips hooks
# entirely, which is not usable here (it excludes OAuth and bills the per-token
# API pool).
#
# CLAUDE_CODE_ENTRYPOINT is the discriminator, measured 2026-08-03 rather than
# assumed:
#     interactive session  -> cli
#     claude -p            -> sdk-cli
# Everything SDK-driven carries an `sdk-` prefix, so match the prefix rather than
# one literal — an sdk-py/sdk-ts caller has the same one-shot shape.
#
# Deliberately a DENY-list, not an allow-list of known-interactive values. An
# unrecognised entrypoint (an IDE frontend, a newer Claude Code, an unset value)
# still arms, because failing that direction costs a wasted background poll,
# while failing the other direction silently disables inter-agent messaging for a
# real user with no error to notice.
if [ -z "${DIALOGUE_REWAKE_FORCE:-}" ]; then
  case "${CLAUDE_CODE_ENTRYPOINT:-}" in
    sdk-*)
      [ -n "${DIALOGUE_REWAKE_DEBUG:-}" ] && \
        echo "$(date +%s) $sid: not arming (entrypoint=$CLAUDE_CODE_ENTRYPOINT)" \
          >>"$DIALOGUE_REWAKE_DEBUG"
      exit 0
      ;;
  esac
fi

wm="${DIALOGUE_REWAKE_WM:-$HOME/.cache/dialogue-wm-$sid}"
mkdir -p "$(dirname "$wm")" 2>/dev/null

# Single-instance guard: if a poller for this session is already alive, stand down.
lock="${wm}.lock"
if [ -f "$lock" ] && kill -0 "$(cat "$lock" 2>/dev/null)" 2>/dev/null; then
  exit 0
fi
echo $$ >"$lock"
trap 'rm -f "$lock"' EXIT INT TERM

# First run: start from "now" so we don't dump the backlog. epoch-ms without GNU
# date %N (this host's date may be BSD): seconds * 1000.
if [ -n "$DIALOGUE_REWAKE_SINCE" ]; then
  since="$DIALOGUE_REWAKE_SINCE"
elif [ -f "$wm" ]; then
  since=$(cat "$wm" 2>/dev/null || echo 0)
else
  since="$(date +%s)000"
  echo "$since" >"$wm"
fi

poll_ms="${DIALOGUE_REWAKE_POLL_MS:-25000}"
max="${DIALOGUE_REWAKE_MAX:-1800}"   # cap a single idle watch at 30 min; Stop re-arms
inc=$((poll_ms / 1000))
[ "$inc" -lt 1 ] && inc=1            # never 0 (would spin); always advance >=1s
waited=0
while [ "$waited" -lt "$max" ]; do
  out=$(agent dialogue poll "cc:$sid" --since "$since" --timeout-ms "$poll_ms" 2>/dev/null)
  msgs=$(printf '%s' "$out" | jq -rc '.messages[]?' 2>/dev/null)
  if [ -n "$msgs" ]; then
    newmax=$(printf '%s' "$out" | jq -r '([.messages[].ts] | max) // empty' 2>/dev/null)
    [ -n "$newmax" ] && printf '%s' "$newmax" >"$wm"
    printf '%s' "$msgs" | jq -r '"DIALOGUE \(.from): \(.content)"'
    exit 2   # wake the model with the message(s) as the rewake system-reminder
  fi
  waited=$((waited + inc))
done
exit 0
