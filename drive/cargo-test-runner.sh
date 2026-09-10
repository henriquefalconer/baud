#!/usr/bin/env bash
# Cargo's test runner. Test binaries inherit the same dynamic four-CPU cap.
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
limit=4
# baud-server embeds baud-multiverse, while the multiverse integration binary can
# run many KVM branches itself. Keep either runtime to the daemon's two-core cap.
case "${1:-}" in
    *baud-server*|*baud_server*|*baud_multiverse*) limit=2 ;;
esac
cpus="$(BAUD_CPU_LIMIT="$limit" "$repo_root/drive/select-free-cpus.sh")"
exec taskset -c "$cpus" "$@"
