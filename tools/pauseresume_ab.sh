#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# SPDX-License-Identifier: Proprietary
#
# pauseresume_ab.sh — the decisive A/B (and an optional C) for the RCB work-clock counter.
#
# baud brackets its retired-conditional-branch counter with pause/resume around every KVM_RUN
# (crates/baud-vcpu/src/linux/mod.rs `run_and_convert_rcb_bracketed`;
# crates/baud-multiverse/src/linux/mod.rs `LinuxBranchCounter`) because exclude_host is claimed
# non-functional on the nested WSL2 dev host (confirm with tools/exclude_probe.c). Separately, two
# call sites were switched from the generic ±1-nondeterministic PERF_COUNT_HW_BRANCH_INSTRUCTIONS
# event to the exact raw 0x11c4 event (confirm with tools/pmucheck.c).
#
#   Arm A — unmodified: raw event + pause/resume.
#   Arm B — raw event, pause/resume NEUTRALIZED (counter free-runs across host dispatch).
#   Arm C — raw event, pause/resume NEUTRALIZED, but exclude_host=1 set on the counter (opt-in):
#           tests whether the "textbook flag" can REPLACE the bracketing during real guest execution.
#
# It runs the enforced-module os_entropy_is_deterministic test (reusing drive/h7-enforced-entropy.sh's
# kernel-module swap; see CLAUDE.md) and reads EVERY gated check the drive runs, not just os_entropy
# (which is too lenient to reveal a small work-clock drift — the stricter
# rdtsc_enforced_regime_is_bit_exact_across_boots is what catches host contamination). All crate edits
# are transient and auto-reverted (trap on EXIT/INT/TERM; refuses to run on a dirty tree).
#
# Usage:
#   N=20 bash tools/pauseresume_ab.sh                 # Arm A vs B (default N=20)
#   RUN_ARM_C=1 ARMC_N=5 bash tools/pauseresume_ab.sh  # also run Arm C (exclude_host)
#
# WARNING (Arm C): if exclude_host really is non-functional here, the counter reads 0, the work-clock
# stalls, and a guest boot HANGS. Arm C is opt-in, bounded by ARMC_TIMEOUT, and force-killed + the
# stock KVM module restored if it stalls. Run it attended.
#
# Verdict is judged on ALL gated checks:
#   Observed A vs B: os_entropy stays 20/20 both ways, but rdtsc_bit_exact PASSes WITH pause/resume
#   and FAILs without it -> pause/resume is LOAD-BEARING for bit-exact work-clock time.

set -uo pipefail

N="${N:-${H7_ENTROPY_REPEATS:-20}}"
RUN_ARM_C="${RUN_ARM_C:-0}"
ARMC_N="${ARMC_N:-5}"
ARMC_TIMEOUT="${ARMC_TIMEOUT:-900}"   # seconds; Arm C is force-killed past this (stall guard)
SUDO_PW="${SUDO_PW:-baud}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT" || { echo "cannot cd to repo root $ROOT" >&2; exit 1; }

FILE="crates/baud-multiverse/src/linux/mod.rs"
DRIVE="drive/h7-enforced-entropy.sh"

# --- preflight -------------------------------------------------------------------------------
command -v git >/dev/null || { echo "git not found" >&2; exit 1; }
git rev-parse --is-inside-work-tree >/dev/null 2>&1 || { echo "not a git repo: $ROOT" >&2; exit 1; }
[[ -f "$FILE"  ]] || { echo "missing $FILE"  >&2; exit 1; }
[[ -f "$DRIVE" ]] || { echo "missing $DRIVE" >&2; exit 1; }
if ! git diff --quiet -- "$FILE" || ! git diff --cached --quiet -- "$FILE"; then
    echo "ERROR: $FILE has uncommitted changes — commit/stash them first (the A/B reverts this file)." >&2
    exit 1
fi

SUDO() { echo "$SUDO_PW" | sudo -S -p '' "$@"; }
NEED_MODULE_RESTORE=0

pass_count() { grep -cE '\[PASS\] run .*os_entropy_is_deterministic' "$1" 2>/dev/null; }
fail_count() { grep -cE '\[FAIL\] run .*os_entropy_is_deterministic' "$1" 2>/dev/null; }

restore_module() {  # force-restore the stock kvm module after a killed arm left it patched
    echo "[ab] force-restoring stock kvm module (a killed arm may have left the patched one loaded)..."
    SUDO pkill -9 -f 'os_entropy_is_deterministic' 2>/dev/null || true
    for _ in $(seq 1 30); do fuser -s /dev/kvm 2>/dev/null || break; sleep 0.5; done
    SUDO rmmod kvm_intel 2>/dev/null || true
    SUDO rmmod kvm 2>/dev/null || true
    SUDO modprobe kvm_intel || echo "  [WARN] modprobe kvm_intel failed — check 'lsmod | grep kvm' by hand" >&2
}

cleanup() {
    git checkout -- "$FILE" 2>/dev/null && echo "[ab] restored $FILE"
    [[ "$NEED_MODULE_RESTORE" == 1 ]] && restore_module
}
trap cleanup EXIT INT TERM

# --- crate edits (content-matched, robust to line drift) -------------------------------------
neutralize_pauseresume() {  # start counter ENABLED + make LinuxBranchCounter::pause/resume no-ops
    sed -i 's#^        counter.disable()?;#        counter.enable()?;#'                    "$FILE"
    sed -i 's#^        let _ = self.counter.disable();#        /* ab: pause neutralized */#' "$FILE"
    sed -i 's#^        let _ = self.counter.enable();#        /* ab: resume neutralized */#'  "$FILE"
    grep -q 'counter.enable()?;' "$FILE" \
      && ! grep -q 'let _ = self.counter.disable();' "$FILE" \
      && ! grep -q 'let _ = self.counter.enable();'  "$FILE"
}
add_exclude_host() {  # set exclude_host=1 on the counter's perf_event_attr (perf-event-open-sys 6.0.0)
    sed -i 's#^        builder.attrs_mut().config = BR_INST_RETIRED_COND;#&\n        builder.attrs_mut().set_exclude_host(1);#' "$FILE"
    grep -q 'set_exclude_host(1);' "$FILE"
}

run_arm() {  # $1 = human label, $2 = log path, $3 = repeats
    echo
    echo "================================================================"
    echo "  Arm $1 — H7_ENTROPY_REPEATS=$3 bash $DRIVE"
    echo "================================================================"
    H7_ENTROPY_REPEATS="$3" bash "$DRIVE" 2>&1 | tee "$2"
}

LOGA="$(mktemp)"; LOGB="$(mktemp)"; LOGC="$(mktemp)"

# --- Arm A: unmodified -----------------------------------------------------------------------
run_arm "A (raw event + pause/resume)" "$LOGA" "$N"

# --- Arm B: neutralize pause/resume, keep the raw event --------------------------------------
echo
echo "[ab] neutralizing pause/resume in $FILE (raw event kept; auto-reverted on exit)"
neutralize_pauseresume || { echo "ERROR: neutralize patch did not apply cleanly (source drifted); reverting." >&2; exit 1; }
git --no-pager diff --stat -- "$FILE"
run_arm "B (raw event, NO pause/resume)" "$LOGB" "$N"
git checkout -- "$FILE"

# --- Arm C (opt-in): exclude_host=1 instead of pause/resume ----------------------------------
C_STATUS="skipped"; C_ALL="-"; C_TSC="-"; PC=0
if [[ "$RUN_ARM_C" == 1 ]]; then
    echo
    echo "[ab] Arm C: neutralize pause/resume AND set exclude_host=1 (can the textbook flag replace"
    echo "     the bracketing?). If exclude_host is non-functional the work-clock stalls and the boot"
    echo "     hangs — bounded to ${ARMC_TIMEOUT}s, then force-killed with a stock-module restore."
    neutralize_pauseresume && add_exclude_host || { echo "ERROR: Arm C patch did not apply; reverting." >&2; exit 1; }
    git --no-pager diff --stat -- "$FILE"

    echo "[ab] compile-checking Arm C (cargo test --no-run)..."
    if ! cargo test -q -p baud-multiverse --lib --no-run >/tmp/ab_armc_build.log 2>&1; then
        echo "  [ab] Arm C DID NOT COMPILE — set_exclude_host may differ in this perf-event(-open-sys)"
        echo "       version; see /tmp/ab_armc_build.log. Skipping the run."
        C_STATUS="compile-fail"
        git checkout -- "$FILE"
    else
        echo
        echo "================================================================"
        echo "  Arm C — H7_ENTROPY_REPEATS=$ARMC_N bash $DRIVE   (bounded ${ARMC_TIMEOUT}s)"
        echo "================================================================"
        NEED_MODULE_RESTORE=1   # if we kill it below, the trap/here restores the module
        H7_ENTROPY_REPEATS="$ARMC_N" timeout --preserve-status -s TERM "$ARMC_TIMEOUT" \
            bash "$DRIVE" > "$LOGC" 2>&1 &
        DPID=$!
        wait "$DPID"; RC=$?
        cat "$LOGC"
        if [[ "$RC" -eq 124 || "$RC" -eq 143 ]]; then
            echo "  [ab] Arm C exceeded ${ARMC_TIMEOUT}s — the work-clock stalled (exclude_host degenerate under guest exec)."
            C_STATUS="stalled"
            restore_module
        else
            C_STATUS="ran"
            # drive completed on its own -> its EXIT trap already restored the module
        fi
        NEED_MODULE_RESTORE=0
        git checkout -- "$FILE"
        PC="$(pass_count "$LOGC")"; PC="${PC:-0}"
        grep -q 'ALL CHECKS PASSED' "$LOGC" && C_ALL=yes || C_ALL=no
        grep -q '\[PASS\] rdtsc_enforced_regime_is_bit_exact_across_boots' "$LOGC" && C_TSC=pass || C_TSC=fail
    fi
fi

trap - EXIT INT TERM
cleanup >/dev/null 2>&1 || true

# --- results ---------------------------------------------------------------------------------
PA="$(pass_count "$LOGA")"; FA="$(fail_count "$LOGA")"
PB="$(pass_count "$LOGB")"; FB="$(fail_count "$LOGB")"
PA=${PA:-0}; FA=${FA:-0}; PB=${PB:-0}; FB=${FB:-0}
grep -q 'ALL CHECKS PASSED' "$LOGA" && A_ALL=yes || A_ALL=no
grep -q 'ALL CHECKS PASSED' "$LOGB" && B_ALL=yes || B_ALL=no
grep -q '\[PASS\] rdtsc_enforced_regime_is_bit_exact_across_boots' "$LOGA" && A_TSC=pass || A_TSC=fail
grep -q '\[PASS\] rdtsc_enforced_regime_is_bit_exact_across_boots' "$LOGB" && B_TSC=pass || B_TSC=fail

echo
echo "================== A/B(/C) RESULT =================="
printf "  Arm A  raw + pause/resume        :  os_entropy=%s/%s  rdtsc_bit_exact=%s  all_checks=%s\n" "$PA" "$N" "$A_TSC" "$A_ALL"
printf "  Arm B  raw, NO pause/resume       :  os_entropy=%s/%s  rdtsc_bit_exact=%s  all_checks=%s\n" "$PB" "$N" "$B_TSC" "$B_ALL"
[[ "$RUN_ARM_C" == 1 ]] && \
printf "  Arm C  raw, exclude_host, no p/r  :  os_entropy=%s/%s  rdtsc_bit_exact=%s  all_checks=%s  [%s]\n" "$PC" "$ARMC_N" "$C_TSC" "$C_ALL" "$C_STATUS"
echo   "---------------------------------------------------"

# --- verdict: is pause/resume redundant? (A vs B) --------------------------------------------
DELTA=$((PA - PB))
if [[ "$PA" -lt $((N - 1)) || "$A_ALL" != yes ]]; then
    echo "  A-vs-B: INCONCLUSIVE — the WITH-pause/resume baseline (Arm A) is not clean"
    echo "          (os_entropy $PA/$N, all_checks=$A_ALL). Fix the baseline before judging."
elif [[ "$A_TSC" == pass && "$B_TSC" != pass ]] || [[ "$A_ALL" == yes && "$B_ALL" != yes ]]; then
    echo "  A-vs-B: pause/resume is LOAD-BEARING — removing it broke a gated check that passed WITH it"
    echo "          (rdtsc_bit_exact ${A_TSC}->${B_TSC}, all_checks ${A_ALL}->${B_ALL}). os_entropy is"
    echo "          too lenient to show it ($PA/$N vs $PB/$N). Keep pause/resume (or replace via Arm C)."
elif [[ "$DELTA" -le 1 && "$B_ALL" == yes ]]; then
    echo "  A-vs-B: pause/resume is REDUNDANT — Arm B matches Arm A on every gated check. The raw"
    echo "          0x11c4 event subsumes it; the bracketing can be simplified/removed."
else
    echo "  A-vs-B: pause/resume is LOAD-BEARING — Arm B degraded (os_entropy $PA->$PB, rdtsc ${A_TSC}->${B_TSC})."
fi

# --- verdict: can exclude_host REPLACE pause/resume? (A vs C) ---------------------------------
if [[ "$RUN_ARM_C" == 1 ]]; then
    case "$C_STATUS" in
      compile-fail)
        echo "  A-vs-C: INCONCLUSIVE — Arm C didn't compile (set_exclude_host name/version mismatch)." ;;
      stalled)
        echo "  A-vs-C: exclude_host is DEGENERATE under guest execution (work-clock stalled -> hang)."
        echo "          It CANNOT replace pause/resume; the code's 'exclude_host non-functional' claim stands." ;;
      ran)
        if [[ "$C_TSC" == pass && "$C_ALL" == yes && "$PC" -ge $((ARMC_N - 1)) ]]; then
            echo "  A-vs-C: exclude_host WORKS under guest execution and REPLACES pause/resume — Arm C passed"
            echo "          every gated check (rdtsc_bit_exact=$C_TSC, os_entropy=$PC/$ARMC_N) with NO bracketing."
            echo "          The code's 'exclude_host broken' claim looks like a misdiagnosis: prefer the"
            echo "          one-line builder.attrs_mut().set_exclude_host(1) and drop run_and_convert_rcb_bracketed."
        else
            echo "  A-vs-C: exclude_host set, but Arm C did NOT cleanly pass (rdtsc_bit_exact=$C_TSC,"
            echo "          os_entropy=$PC/$ARMC_N, all_checks=$C_ALL) — it does not replace pause/resume; keep it."
        fi ;;
      *) : ;;
    esac
fi
echo "  logs: A=$LOGA  B=$LOGB $( [[ "$RUN_ARM_C" == 1 ]] && echo "C=$LOGC" )"
echo "==================================================="
