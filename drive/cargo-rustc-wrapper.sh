#!/usr/bin/env bash
# Cargo invokes this wrapper for every compiler process. Keep compiler work on
# the four CPUs that were least busy when this invocation started.
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cpus="$(BAUD_CPU_LIMIT=4 "$repo_root/drive/select-free-cpus.sh")"
exec taskset -c "$cpus" "$@"
