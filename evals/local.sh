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
#
# **8000 was too tight and cost a task outright.** On 2026-09-20
# `ambiguous-config` spent two consecutive completions of exactly 8000
# tokens entirely inside reasoning, emitted no content either time, and
# the branch went quiet with the task untouched — a FAIL that was our
# cap, not the model. `dead-code-sweep` lost a reply the same way. The
# models on this box are configured with `--ctx-size 65536`
# (`evals/lanmodels.sh` prints the preset), and a request's document is
# ~8-30KB, so this leaves room for a long trace without ever reaching
# the context end.
export DEEPSEEK_MAX_TOKENS="${DEEPSEEK_MAX_TOKENS:-24000}"

# **`high` is not a level this box understands.** The harness defaults
# to it (`openai_completions.rs`'s DEFAULT_EFFORT), which is right for
# the OpenAI-shaped paid endpoints and wrong here: Qwen3.8-27B's chat
# template accepts `xhigh` (its own default), `medium` and `low`, and
# rejects anything else outright —
#
#   Jinja Exception: Unexpected reasoning effort high.
#   Supported types are xhigh (default), medium, and low.
#
# which arrives as a 500 and reads, up in drive.py, as "agent exited 1
# without writing a program": a whole suite of NO RUNs for a setting
# neither the harness nor the driver has any business knowing. The box
# is this file's business, so it is set here.
#
# `medium` rather than `xhigh` because this box serves one slot and the
# arms are about which verbs the model reaches for, not how hard it
# thinks. Override for a thinking-level sweep.
export DEEPSEEK_REASONING_EFFORT="${DEEPSEEK_REASONING_EFFORT:-medium}"

if ! curl -sf --max-time 10 "$LOCAL_BASE_URL/models" >/dev/null; then
  echo "evals/local.sh: no model server at $LOCAL_BASE_URL" >&2
  echo "  set LOCAL_BASE_URL, or start the box." >&2
  exit 2
fi

# **Shift the needle off before looking for it.** `have --jobs "$@"`
# put `--jobs` into `$*` as well as `$1`, so the pattern always matched
# its own needle and neither default below was ever applied: every run
# through this script took drive.py's `--jobs 4` against a box that
# serves one slot, and its 900s timeout instead of the 3000s this file
# asks for. Four runs queueing behind each other and being killed at 900
# seconds is what "the LAN model is too slow to measure with" was
# partly made of.
ORIGINAL_ARGS=("$@")

have() {
  local needle=$1
  shift
  case " $* " in *" $needle "*) return 0 ;; esac
  return 1
}
extra=()
have --jobs "$@" || extra+=(--jobs 1)
have --timeout "$@" || extra+=(--timeout 3000)

# **One GPU, so one run.** `--jobs 1` is the default above, but a
# caller can pass their own — and this box is a single GPU serving a
# single slot, so anything above 1 does not run in parallel, it
# queues. What that produces is not a slower suite, it is a wrong one:
# every run's wall clock includes the time it spent waiting behind the
# others, and runs get killed at the timeout for a queue they were
# never told about. That is most of what the discredited
# "~450s/program" figure was made of, and the bug that caused it was in
# this very function. A default can be overridden by accident; this
# cannot.
while [ $# -gt 0 ]; do
  case $1 in
    --jobs)
      [ "${2:-1}" -gt 1 ] 2>/dev/null && {
        echo "evals/local.sh: --jobs $2 — this box is one GPU serving one slot," >&2
        echo "  so concurrent runs queue and every timing is wrong. Use --jobs 1." >&2
        exit 2
      }
      ;;
    --jobs=*)
      [ "${1#--jobs=}" -gt 1 ] 2>/dev/null && {
        echo "evals/local.sh: ${1} — this box is one GPU serving one slot," >&2
        echo "  so concurrent runs queue and every timing is wrong. Use --jobs 1." >&2
        exit 2
      }
      ;;
  esac
  shift
done
set -- "${ORIGINAL_ARGS[@]}"

echo "evals/local.sh: $LOCAL_MODEL at $LOCAL_BASE_URL" >&2
exec python3 "$(dirname "$0")/drive.py" "${extra[@]}" "$@"
