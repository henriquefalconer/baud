#!/usr/bin/env bash
# Pick the least-busy CPUs visible to this process.
# Output is a taskset-compatible list, for example: 2,5,6,7.
set -euo pipefail

want="${BAUD_CPU_LIMIT:-4}"
case "$want" in ''|*[!0-9]*) want=4 ;; esac

allowed=$(awk '/^Cpus_allowed_list:/ {print $2}' /proc/self/status)
[[ -n "$allowed" ]] || allowed=$(nproc)

read_stats() {
    awk '/^cpu[0-9]+[[:space:]]/ {
        idle=$5+$6
        total=0
        for (i=2; i<=NF; i++) total += $i
        print substr($1,4), idle, total
    }' /proc/stat
}

# Expand the kernel's compact CPU list without depending on nproc's view of cpus.
expand_list() {
    local part lo hi cpu
    IFS=',' read -ra parts <<< "$1"
    for part in "${parts[@]}"; do
        if [[ "$part" == *-* ]]; then
            lo=${part%-*}; hi=${part#*-}
            for ((cpu=lo; cpu<=hi; cpu++)); do printf '%s\n' "$cpu"; done
        else
            printf '%s\n' "$part"
        fi
    done
}

mapfile -t cpus < <(expand_list "$allowed")
(( ${#cpus[@]} > 0 )) || { echo 'CPU affinity mask is empty' >&2; exit 1; }
declare -A permitted
for cpu in "${cpus[@]}"; do permitted[$cpu]=1; done

# Compare two /proc/stat samples. The idle delta is the actual idle time during the
# interval, so a CPU that was busy before this command started is not penalized forever.
declare -A idle0 total0
after="$(read_stats)"
while read -r cpu idle total; do idle0[$cpu]=$idle; total0[$cpu]=$total; done <<< "$after"
sleep 0.15
after="$(read_stats)"

ranked=$(while read -r cpu idle total; do
    [[ -n "${idle0[$cpu]+x}" && -n "${permitted[$cpu]+x}" ]] || continue
    di=$((idle-idle0[$cpu])); dt=$((total-total0[$cpu]))
    (( dt > 0 )) || dt=1
    # Sort by idle fraction, with the CPU number as a stable tie-breaker.
    printf '%s %s %s\n' "$cpu" $((di*100000/dt)) "$cpu"
done <<< "$after" | sort -k2,2nr -k3,3n)

chosen=()
while read -r cpu _score _tie; do
    [[ -n "$cpu" ]] || continue
    chosen+=("$cpu")
    (( ${#chosen[@]} >= want )) && break
done <<< "$ranked"
if (( ${#chosen[@]} == 0 )); then chosen=("${cpus[@]:0:want}"); fi
(IFS=','; echo "${chosen[*]}")
