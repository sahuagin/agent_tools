#!/bin/sh
# SessionStart hook: register this cc session on the mu-dialogue channel and tell
# it how to auto-receive / reply. Registration (a non-blocking poll) makes the
# session reachable by peers even before it does anything. The additionalContext
# informs it of the watcher + reply commands; starting the persistent Monitor is
# left to the session's judgement (only worth it when coordinating with peers),
# so a focused unrelated session isn't forced to spin one up.
#
# Reads session_id from the hook's stdin JSON, falling back to the env var.
sid=$(jq -r '.session_id // empty' 2>/dev/null)
[ -n "$sid" ] || sid="$CLAUDE_CODE_SESSION_ID"
[ -n "$sid" ] || exit 0

# Anchor the rewake listener's watermark to session-START time. The Stop-hook
# poller only arms when the session first goes idle and otherwise seeds its
# watermark to *that* moment, so any message arriving between session start and
# first idle falls behind the waterline and is never surfaced. Seeding here
# closes that opening blind window. Only seed when absent — never clobber a
# watermark a running poller has already advanced (e.g. on resume/clear).
# Path and epoch-ms form must match dialogue-rewake.sh exactly.
wm="${DIALOGUE_REWAKE_WM:-$HOME/.cache/dialogue-wm-$sid}"
mkdir -p "$(dirname "$wm")" 2>/dev/null
[ -f "$wm" ] || printf '%s' "$(date +%s)000" >"$wm"

# Register presence (best-effort, non-blocking — never hold up session start).
agent dialogue poll "cc:$sid" --timeout-ms 0 >/dev/null 2>&1 || true

# Inject guidance. jq -Rs encodes the string as a safe JSON value (handles quoting).
ctx="Inter-agent dialogue is available via the deployed 'agent dialogue' CLI; you are registered on the channel as cc:$sid. Inbound messages from other agents are surfaced to you AUTOMATICALLY by the Stop-hook listener (no action needed — when a peer writes, you are woken with the message, even while idle). Reply with:  agent dialogue say --from cc:$sid --to <peer-id> --content '...'  . List who is on the channel:  agent dialogue peers  ."
printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":%s}}\n' "$(printf '%s' "$ctx" | jq -Rs .)"
