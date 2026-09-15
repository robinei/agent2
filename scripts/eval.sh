#!/usr/bin/env bash
# Runs `agent eval` confined under bwrap.
#
# The harness itself (agent/src/eval/) has no idea whether it is
# sandboxed -- it just does file I/O and shells out, the same as any
# other program. Confining the process is this script's job, decided
# before the binary ever runs, per docs/DESIGN.md's "Confinement, not
# permission": no allowlist, no per-call check, just a boundary the
# whole process runs inside. This is the only place `bwrap` is named.
#
# What's bound and what isn't:
#   --ro-bind / /     the entire host filesystem, read-only -- the agent
#                      binary, bash, coreutils, CA certs, DNS config all
#                      need to be reachable, but nothing under this bind
#                      can be written to.
#   --dev /dev         a minimal, private /dev.
#   --proc /proc       a private /proc for the confined process's own
#                       namespace.
#   --tmpfs /tmp        a private, writable scratch filesystem -- this is
#                       where the harness creates its per-task sandbox
#                       directories (`tasks::make_sandbox`), so every
#                       eval task's file writes/deletes land here and
#                       nowhere on the real machine.
#   --unshare-all       new pid/net/ipc/uts/cgroup namespaces (bwrap
#                       manages the user namespace itself).
#   --share-net         ...except networking: DeepSeek is an HTTPS API
#                       call, so the sandbox keeps the host's network.
#
# Usage: scripts/eval.sh [--experimental]
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cargo build --manifest-path "$repo_root/Cargo.toml" -p agent --bin agent

exec bwrap \
  --ro-bind / / \
  --dev /dev \
  --proc /proc \
  --tmpfs /tmp \
  --unshare-all \
  --share-net \
  --die-with-parent \
  --new-session \
  --chdir "$repo_root" \
  "$repo_root/target/debug/agent" eval "$@"
