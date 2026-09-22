#!/usr/bin/env bash
# One module's check. Slow on purpose: the point of this task is that
# doing nine of these in sequence costs nine times as long as doing
# them at once, and that difference is the whole measurement.
mod="$1"
sleep 6
case "$mod" in
  beta)  echo "FAIL $mod: unresolved symbol 'frobnicate'"; exit 1 ;;
  eta)   echo "FAIL $mod: type error at line 44"; exit 1 ;;
  *)     echo "OK $mod"; exit 0 ;;
esac
