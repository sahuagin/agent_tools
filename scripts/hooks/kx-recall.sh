#!/usr/bin/env bash
# kx-recall — UserPromptSubmit hook.
#
# Auto-surfaces relevant prior work (episodic memory + kx document-index entries)
# by running a SEMANTIC recall on the user's actual prompt and injecting only the
# above-threshold hits. This is the keystone of the knowledge-map: a cheap query
# matched to what was just asked, NOT a context dump loaded every turn. Stays
# silent when nothing clears the bar.
#
# Tunables (env): KX_MIN_SCORE (default 0.60), KX_MAX_HITS (4), KX_RECALL_K (6).
# Disable: remove the UserPromptSubmit entry in ~/.claude/settings.json (/hooks).
set -uo pipefail

MIN_SCORE="${KX_MIN_SCORE:-0.60}"
MAX_HITS="${KX_MAX_HITS:-4}"
K="${KX_RECALL_K:-6}"
AGENT="$HOME/.local/bin/agent"

input="$(cat)"
prompt="$(printf '%s' "$input" | jq -r '.prompt // empty' 2>/dev/null)"
[ -z "$prompt" ] && exit 0

# Semantic recall spans ALL types — episodic memory and kx reference entries
# come back ranked together (filter by tag/type only when you want one).
raw="$("$AGENT" memory recall "$prompt" --k "$K" --json 2>/dev/null)" || exit 0
[ -z "$raw" ] && exit 0

block="$(printf '%s' "$raw" | jq -r --argjson min "$MIN_SCORE" --argjson max "$MAX_HITS" '
  (.results // [])
  | map(select((.score // 0) >= $min))
  | .[0:$max]
  | map("- [" + ((.score * 100) | floor | tostring) + "] " + .name + " (" + .type + ") — "
        + ((.description // "") | gsub("\n"; " ")))
  | join("\n")
' 2>/dev/null)" || exit 0
[ -z "$block" ] && exit 0

ctx="Possibly-relevant prior work, auto-recalled from memory + the kx document index (open the entry/doc if useful; ignore if not):
$block"

jq -n --arg c "$ctx" \
  '{hookSpecificOutput: {hookEventName: "UserPromptSubmit", additionalContext: $c}, suppressOutput: true}' \
  2>/dev/null || exit 0
