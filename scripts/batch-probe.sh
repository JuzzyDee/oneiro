#!/usr/bin/env bash
# Throwaway probe: does the Anthropic Batch API work with our token + account,
# including forced tool_choice (the exact mechanism encode relies on)?
#
#   export ANTHROPIC_API_KEY='sk-ant-api...'   # the same secret the worker holds
#   bash scripts/batch-probe.sh
#
# PASS: submit returns a "msgbatch_..." id, poll reaches "ended", and the result
#       contains a tool_use block with input {"ok": true}.
# FAIL: submit returns 4xx/403 — Batch (or tools-in-batch) not available on this
#       key/account. Read the error body it prints.
#
# Mirrors src/worker_encode.rs auth: sk-ant-api… -> x-api-key, else Bearer.
# Safe to delete after.
set -euo pipefail

: "${ANTHROPIC_API_KEY:?export ANTHROPIC_API_KEY=sk-ant-... first}"

BASE="https://api.anthropic.com/v1/messages/batches"
if [[ "$ANTHROPIC_API_KEY" == sk-ant-api* ]]; then
  AUTH=(-H "x-api-key: ${ANTHROPIC_API_KEY}")
else
  AUTH=(-H "Authorization: Bearer ${ANTHROPIC_API_KEY}")
fi
AUTH+=(-H "anthropic-version: 2023-06-01" -H "content-type: application/json")

echo "── submitting batch (forced tool_choice) ──"
SUBMIT=$(curl -sS -X POST "$BASE" "${AUTH[@]}" -d '{
  "requests": [{
    "custom_id": "probe-1",
    "params": {
      "model": "claude-haiku-4-5-20251001",
      "max_tokens": 64,
      "tools": [{
        "name": "probe_tool",
        "description": "Acknowledge the probe.",
        "input_schema": {
          "type": "object",
          "properties": { "ok": { "type": "boolean" } },
          "required": ["ok"]
        }
      }],
      "tool_choice": { "type": "tool", "name": "probe_tool" },
      "messages": [{ "role": "user", "content": "Call probe_tool with ok set to true." }]
    }
  }]
}')
echo "$SUBMIT"

BATCH_ID=$(printf '%s' "$SUBMIT" | grep -o '"id":"msgbatch_[^"]*"' | head -1 | sed 's/.*"\(msgbatch_[^"]*\)".*/\1/' || true)
if [ -z "${BATCH_ID:-}" ]; then
  echo
  echo "!! No batch id in the response above — submit failed. Batch (or this key) is unsupported."
  exit 1
fi
echo
echo "── batch id: $BATCH_ID — polling (max ~5 min) ──"

for i in $(seq 1 30); do
  STATUS=$(curl -sS "$BASE/$BATCH_ID" "${AUTH[@]}")
  if printf '%s' "$STATUS" | grep -q '"processing_status":"ended"'; then
    echo "── ended (~$((i*10))s) — fetching results ──"
    curl -sS "$BASE/$BATCH_ID/results" "${AUTH[@]}"
    echo
    echo "── done. tool_use with {\"ok\":true} above == full PASS ──"
    exit 0
  fi
  echo "  [$i] in_progress… sleeping 10s"
  sleep 10
done
echo "!! Still not ended after 5 min — unusual for a one-line batch."
