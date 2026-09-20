#!/usr/bin/env bash
# Run the eval suite against the LAN model instead of a paid endpoint.
#
# The paid arm is the expensive habit, and most of what an arm is for —
# does the model reach for this verb, does the report read right, does
# the edit land — shows up in one or two runs of a free model. One
# night of paid arms cost 1.5M uncached prompt and 2.8M output tokens
# across 336 runs; this costs electricity.
#
#   bash evals/local.sh --tasks build-stats,delegate-direct --repeat 1
#
# Everything after the script name goes straight to drive.py, so
# --keep, --out and the rest work as they always did.
#
# The trade is wall clock: roughly 13x slower per run than
# deepseek-v4-flash (796s against ~60s on build-stats), so this wants
# one or two runs in the background rather than --repeat 3 across the
# suite. --jobs defaults to 1 here because llama-server serves one slot
# and parallel requests only queue.
set -euo pipefail

: "${LOCAL_BASE_URL:=http://192.168.1.216:8080/v1}"
: "${LOCAL_MODEL:=Qwen3.8-27B}"

# A key is required by the client and ignored by llama-server.
export DEEPSEEK_API_KEY="${DEEPSEEK_API_KEY:-none}"
export DEEPSEEK_BASE_URL="$LOCAL_BASE_URL"
export DEEPSEEK_MODEL="$LOCAL_MODEL"
# Bounds a reasoning trace that would otherwise run to the context end
# and spend a whole run's timeout on one program.
export DEEPSEEK_MAX_TOKENS="${DEEPSEEK_MAX_TOKENS:-8000}"

if ! curl -sf --max-time 10 "$LOCAL_BASE_URL/models" >/dev/null; then
  echo "evals/local.sh: no model server at $LOCAL_BASE_URL" >&2
  echo "  set LOCAL_BASE_URL, or start the box." >&2
  exit 2
fi

have() { case " $* " in *" $1 "*) return 0;; esac; return 1; }
extra=()
have --jobs "$@" || extra+=(--jobs 1)
have --timeout "$@" || extra+=(--timeout 3000)

echo "evals/local.sh: $LOCAL_MODEL at $LOCAL_BASE_URL" >&2
exec python3 "$(dirname "$0")/drive.py" "${extra[@]}" "$@"
