#!/usr/bin/env bash
# oneiro-orient.sh — Claude Code hook script (CLA-105).
#
# Called from SessionStart and PreCompact hooks in ~/.claude/settings.json.
# Fetches the orientation payload from the deployed Oneiro worker and
# prints it to stdout — Claude Code injects stdout into the session's
# system context before the first user message (or after compaction).
#
# Why this exists: fresh instances often don't surface or call
# `recall_orient` before evaluating tools. Even when they do, the
# SKILL.md instruction loses against the system prompt's auto-memory
# section. The hook makes orientation availability structural rather
# than discretionary — orientation arrives before any tool evaluation
# fires, so the substance is already in scope when the model decides
# what to do.
#
# Configuration (read from environment):
#   ONEIRO_WORKER_URL    — e.g. https://oneiro.<subdomain>.workers.dev
#   ONEIRO_ORIENT_TOKEN  — service API key OR OAuth bearer (optional;
#                          if unset, falls back to keychain lookup)
#
# Keychain fallback (macOS):
#   security add-generic-password -s "oneiro-orient" -a "$USER" -w "<key>"
#
# Failure semantics: any error (missing config, unreachable worker,
# auth rejection, etc.) exits 0 with no stdout. The session continues
# as if the hook hadn't run — never break a session because Oneiro is
# down or misconfigured.

set -u  # error on undefined vars; not -e (we want to handle errors ourselves)

# 1. Worker URL — required. No sensible default; without it we have nothing to call.
WORKER_URL="${ONEIRO_WORKER_URL:-}"
[ -z "$WORKER_URL" ] && exit 0

# 2. Auth token — env first (explicit), then macOS keychain (preferred for
#    persistence without exposing to process listings). The keychain lookup
#    pins the account to $USER so an entry from a different login user (or
#    a stale unrelated `oneiro-orient` entry) can't shadow the right one.
TOKEN="${ONEIRO_ORIENT_TOKEN:-}"
if [ -z "$TOKEN" ] && command -v security >/dev/null 2>&1; then
    TOKEN=$(security find-generic-password -s "oneiro-orient" -a "${USER:-}" -w 2>/dev/null || true)
fi
[ -z "$TOKEN" ] && exit 0

# 3. Fetch. `--fail` makes curl return non-zero on HTTP >= 400 so the
#    `||` handles auth/server errors as silent no-ops. `--max-time` so
#    a slow worker can't stall session startup indefinitely. stderr is
#    swallowed deliberately (the failure semantics are silent no-op),
#    so `--show-error` would be a no-op and is omitted.
curl --silent --fail --max-time 10 \
    --header "Authorization: Bearer ${TOKEN}" \
    "${WORKER_URL%/}/orientation" 2>/dev/null \
    || exit 0
