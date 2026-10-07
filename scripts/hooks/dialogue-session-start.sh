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

# Hold an etcd-lease presence key for as long as this session lives (mu-34mgd).
#
# The poll above only refreshes mu-dialogue's ACTIVITY-derived presence, whose
# default TTL is an hour — and nothing refreshes it once dialogue-rewake.sh hits
# its 30-minute idle cap, because only Stop re-arms that poller. So a
# live-but-quiet session read as DEAD: measured 2026-10-07, the IRC gateway saw
# the peer leave discovery, quit its puppet, and the operator's reply to a
# session that was alive the whole time hit "No such nick".
#
# A lease fixes the direction that matters. It is held while this process lives
# and expires on its own when it does not, which is what presence.rs means by
# "the lease IS the liveness proof". The mu daemon already writes its keys; this
# is the cc writer that module's docs name and that was never built.
#
# Backgrounded and silent: SessionStart must not delay the session, and this is
# strictly optional. The holder locates the session process itself, single-
# instances per session, and exits quietly when presence is unconfigured or etcd
# is unreachable — leaving activity-derived presence exactly as it was.
"$HOME/.claude/hooks/dialogue-presence.py" --session-id "$sid" --watch-pid "$PPID" \
  >/dev/null 2>&1 &

# Inject guidance. jq -Rs encodes the string as a safe JSON value (handles quoting).
ctx="Inter-agent dialogue is available via the deployed 'agent dialogue' CLI; you are registered on the channel as cc:$sid. Inbound messages from other agents are surfaced to you AUTOMATICALLY by the Stop-hook listener (no action needed — when a peer writes, you are woken with the message, even while idle). Reply with:  agent dialogue say --from cc:$sid --to <peer-id> --content '...'  . List who is on the channel:  agent dialogue peers  ."
printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":%s}}\n' "$(printf '%s' "$ctx" | jq -Rs .)"
