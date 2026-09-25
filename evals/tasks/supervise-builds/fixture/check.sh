#!/usr/bin/env bash
# One module's check. Slow on purpose: the point of this task is that
# doing nine of these in sequence costs nine times as long as doing
# them at once, and that difference is the whole measurement.
#
# **It takes `beta` or `beta.mod`, and refuses anything else.** The
# prompt says "every *.mod file here is a module", so both readings of
# `<module>` are fair and a checker that only knew one of them was
# grading the guess instead of the work: on 2026-09-25 a run checked
# all nine as `alpha.mod … zeta.mod`, every one fell through to the
# catch-all, and it reported "All 9 modules pass" — a confident wrong
# answer, with the task's real question (does it fan out?) never
# reached. An unknown module is now an error, because a checker that
# says OK for a module it has never heard of cannot be trusted when it
# says OK for one it has.
mod="${1%.mod}"
if [ -z "$mod" ]; then
  echo "usage: ./check.sh <module>" >&2
  exit 2
fi
sleep 6
case "$mod" in
  alpha|gamma|delta|epsilon|zeta|theta|iota)
    echo "OK $mod"; exit 0 ;;
  beta)  echo "FAIL $mod: unresolved symbol 'frobnicate'"; exit 1 ;;
  eta)   echo "FAIL $mod: type error at line 44"; exit 1 ;;
  *)     echo "no such module: $mod" >&2; exit 2 ;;
esac
