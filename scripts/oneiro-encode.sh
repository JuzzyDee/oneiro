#!/usr/bin/env bash
# oneiro-encode.sh — Claude Code PostCompact hook (CLA-116).
#
# Captures the compaction summary as a raw episodic in Oneiro. Thin: it POSTs
# the summary text to /encode; the worker stores it unembedded and the nightly
# Stage-1 pass distils + embeds the clean output (so the muddy multi-topic
# summary is never embedded). Distillation is NOT done here.
#
# Wired from a PostCompact hook in ~/.claude/settings.json. Receives the hook
# payload as JSON on stdin — we read .transcript_path and .cwd from it.
#
# Configuration (read from environment):
#   ONEIRO_WORKER_URL    — e.g. https://oneiro.<subdomain>.workers.dev
#   ONEIRO_HOOK_TOKEN    — Hook-role key (needs the `encode` scope). Falls back
#     / ONEIRO_ORIENT_TOKEN  to ONEIRO_ORIENT_TOKEN, then the macOS keychain
#                            (service "oneiro-orient"), so one key serves both
#                            the orient and encode hooks.
#
# DEFAULT-DENY capture: this fires ONLY for projects you've explicitly marked
# personal-safe with a `.oneiro-capture` marker file (in the working dir or any
# ancestor). Any project without the marker — client/work code like HazChat —
# is never captured. Zero-leak by construction; no reliance on redaction.
# (Capturing work contexts safely, with redaction, is a separate future
# capability — see docs/orientation-redesign.md.)
#
# Failure semantics: ANY problem (missing config, no marker, unreachable
# worker, auth rejection, no summary) exits 0 silently. A capture hook must
# never break a session or surface noise.

set -u  # error on undefined vars; not -e — we handle errors ourselves.

# --- DEBUG (CLA-116 diagnostics) — appends a forensic trail ONLY when the flag
#     file ~/.claude/.oneiro-encode-debug exists. Off by default; `rm` the flag
#     to silence. Lets us see, on the next compaction, whether the hook fired at
#     all and exactly where it bailed (the silent exits leave no other trace).
DBG_LOG="${HOME}/.claude/oneiro-encode-debug.log"
dbg() {
    [ -e "${HOME}/.claude/.oneiro-encode-debug" ] || return 0
    printf '%s  %s\n' "$(date -u '+%FT%TZ')" "$1" >> "$DBG_LOG" 2>/dev/null || true
}
dbg "fired pid=$$"

# 0. Hard dependencies. Without them there's no clean path — silent no-op.
command -v jq   >/dev/null 2>&1 || { dbg "exit: no jq"; exit 0; }
command -v curl >/dev/null 2>&1 || { dbg "exit: no curl"; exit 0; }

# 1. Hook payload on stdin → transcript path + cwd.
INPUT=$(cat)
[ -z "$INPUT" ] && { dbg "exit: empty stdin"; exit 0; }
TRANSCRIPT=$(printf '%s' "$INPUT" | jq -r '.transcript_path // empty' 2>/dev/null)
CWD=$(printf '%s' "$INPUT" | jq -r '.cwd // empty' 2>/dev/null)
if [ -z "$TRANSCRIPT" ] || [ ! -f "$TRANSCRIPT" ]; then
    dbg "exit: no transcript file (path='$TRANSCRIPT')"; exit 0
fi
dbg "transcript=$TRANSCRIPT cwd=$CWD"

# 2. Default-deny context gate. Walk up from cwd looking for a `.oneiro-capture`
#    marker. No marker anywhere up the tree → not personal-safe → never send.
DIR="${CWD:-$PWD}"
marker=0
while [ -n "$DIR" ] && [ "$DIR" != "/" ]; do
    if [ -e "$DIR/.oneiro-capture" ]; then marker=1; break; fi
    DIR=$(dirname "$DIR")
done
[ "$marker" -eq 0 ] && { dbg "exit: no .oneiro-capture marker under '$CWD'"; exit 0; }
dbg "marker ok"

# 3. Worker URL + token (mirrors oneiro-orient.sh; one Hook-role key).
WORKER_URL="${ONEIRO_WORKER_URL:-}"
[ -z "$WORKER_URL" ] && { dbg "exit: no ONEIRO_WORKER_URL"; exit 0; }
TOKEN="${ONEIRO_HOOK_TOKEN:-${ONEIRO_ORIENT_TOKEN:-}}"
if [ -z "$TOKEN" ] && command -v security >/dev/null 2>&1; then
    TOKEN=$(security find-generic-password -s "oneiro-orient" -a "${USER:-}" -w 2>/dev/null || true)
fi
[ -z "$TOKEN" ] && { dbg "exit: no token"; exit 0; }

# 4. Extract the most recent compaction summary. Verified format: a transcript
#    line with top-level `isCompactSummary: true` and `.message.content` as a
#    string. grep pre-filters candidate lines fast; jq's `select` re-checks the
#    real field (so a line that merely *mentions* the marker in its text can't
#    sneak through), and `strings` guards against a future array-shaped content.
SUMMARY=$(grep -F '"isCompactSummary":true' "$TRANSCRIPT" 2>/dev/null \
    | jq -rs '[.[] | select(.isCompactSummary == true) | .message.content | strings] | last // empty' 2>/dev/null)
[ -z "$SUMMARY" ] && { dbg "exit: empty summary (no isCompactSummary line in transcript yet?)"; exit 0; }
dbg "summary extracted len=${#SUMMARY}"

# 5. POST as a raw episodic. Tagged "compaction" so the nightly pass and the
#    audit trail can see the source. jq builds the body so the (large,
#    newline-laden) summary is escaped correctly.
BODY=$(jq -nc --arg c "$SUMMARY" '{content: $c, tags: ["compaction"]}')
HTTP=$(curl --silent --output /dev/null --write-out '%{http_code}' --max-time 10 \
    --header "Authorization: Bearer ${TOKEN}" \
    --header "Content-Type: application/json" \
    --data "$BODY" \
    "${WORKER_URL%/}/encode" 2>/dev/null || echo "000")
dbg "posted http=$HTTP"
case "$HTTP" in 200|201) ;; *) exit 0 ;; esac
