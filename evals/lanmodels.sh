#!/usr/bin/env bash
# What the LAN box has loaded, and enough control to benchmark fairly.
#
# The box runs llama-server's multi-model router, not llama-swap: it
# holds several models resident, loads one on the first request that
# names it, and puts each to sleep after `--sleep-idle-seconds` (300).
# So a cold model's first request pays the load, and a model you
# measured ten minutes ago may have gone back to sleep since.
#
#   evals/lanmodels.sh                    # status of every model
#   evals/lanmodels.sh warm <id>          # load it and time the wait
#   evals/lanmodels.sh unload <id>        # free it
#
# **Use the id exactly as `status` prints it.** The short aliases in
# `/v1/models` are real entries with their own presets — `Qwen3.6-35B`
# is the MTP/speculative build at ctx 65536, a different thing from
# `unsloth/Qwen3.6-35B-A3B-GGUF:Q4_K_M` — and a request naming one
# while the other is resident triggers a fresh load.
set -uo pipefail
: "${LAN:=http://192.168.1.216:8080}"
cmd=${1:-status}

case "$cmd" in
  status)
    curl -s --max-time 10 "$LAN/models" | python3 -c '
import json, sys
for m in json.load(sys.stdin)["data"]:
    st = m.get("status", {})
    args = st.get("args", [])
    path = ""
    for i, a in enumerate(args):
        if a == "--model" and i + 1 < len(args):
            path = args[i + 1].split("/")[-1]
    mtp = "+mtp" if "--spec-type" in args else ""
    value = st.get("value", "?")
    print("%-9s %-42s %-50s %s" % (value, m["id"], path[:48], mtp))'
    ;;
  warm)
    id=${2:?which model}
    s=$(date +%s)
    curl -s --max-time 900 "$LAN/v1/chat/completions" -H 'Content-Type: application/json' \
      -d "{\"model\":\"$id\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}],\"max_tokens\":4}" \
      > /dev/null
    echo "warm after $(( $(date +%s) - s ))s"
    ;;
  unload)
    id=${2:?which model}
    curl -s --max-time 30 -X POST -H 'Content-Type: application/json' \
      -d "{\"model\":\"$id\"}" "$LAN/models/unload"
    echo
    ;;
  *) echo "usage: $0 [status|warm <id>|unload <id>]" >&2; exit 2 ;;
esac
