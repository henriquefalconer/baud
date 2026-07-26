#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# SPDX-License-Identifier: Proprietary
#
# pauseresume_ab.sh — the decisive A/B: is the RCB pause/resume bracketing redundant now that the
# raw BR_INST_RETIRED.COND (0x11c4) work-clock event is used?
#
# baud brackets its retired-conditional-branch counter with pause/resume around every KVM_RUN
# (crates/baud-vcpu/src/linux/mod.rs `run_and_convert_rcb_bracketed`;
# crates/baud-multiverse/src/linux/mod.rs `LinuxBranchCounter`) because exclude_host is
# non-functional on the nested WSL2 dev host (confirm with tools/exclude_probe.c). Separately, two
# call sites were switched from the generic ±1-nondeterministic PERF_COUNT_HW_BRANCH_INSTRUCTIONS
# event to the exact raw 0x11c4 event (confirm with tools/pmucheck.c). The pause/resume win was
# only ever measured under the WRONG generic event and never re-measured without pause/resume, so
# whether the bracketing is still load-bearing — or now redundant given the exact raw event — is
# untested. This script settles it, with NO manual editing.
#
# It runs the enforced-module os_entropy_is_deterministic test N times WITH pause/resume (Arm A),
# then TRANSIENTLY neutralizes pause/resume in the baud-multiverse crate (raw event kept intact),
# runs N times again (Arm B), and ALWAYS reverts the crate (trap on exit; refuses to run on a dirty
# tree). It reuses drive/h7-enforced-entropy.sh for the kernel-module swap dance (see CLAUDE.md), so
# it needs the same prerequisites (patched kvm modules buildable, sudo password `baud`).
#
# Usage:   N=20 bash tools/pauseresume_ab.sh        # default N=20; bump to 50 if borderline
#
# Verdict:
#   Arm B PASS ≈ Arm A PASS (both near N)  -> pause/resume is REDUNDANT given the raw event.
#   Arm B PASS materially lower            -> pause/resume is LOAD-BEARING; keep it.

set -uo pipefail

N="${N:-${H7_ENTROPY_REPEATS:-20}}"

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

# Refuse on a dirty target file: the revert is `git checkout -- $FILE`, which would clobber
# uncommitted edits. Keep the A/B honest and safe.
if ! git diff --quiet -- "$FILE" || ! git diff --cached --quiet -- "$FILE"; then
    echo "ERROR: $FILE has uncommitted changes — commit/stash them first (the A/B reverts this file)." >&2
    exit 1
fi

pass_count() { grep -cE '\[PASS\] run .*os_entropy_is_deterministic' "$1" 2>/dev/null; }
fail_count() { grep -cE '\[FAIL\] run .*os_entropy_is_deterministic' "$1" 2>/dev/null; }

run_arm() { # $1 = human label, $2 = log path
    echo
    echo "================================================================"
    echo "  Arm $1 — H7_ENTROPY_REPEATS=$N bash $DRIVE"
    echo "================================================================"
    H7_ENTROPY_REPEATS="$N" bash "$DRIVE" 2>&1 | tee "$2"
}

restore() { git checkout -- "$FILE" 2>/dev/null && echo "[ab] restored $FILE"; }

LOGA="$(mktemp)"
LOGB="$(mktemp)"

# --- Arm A: unmodified (raw event + pause/resume) --------------------------------------------
run_arm "A (raw event + pause/resume)" "$LOGA"

# --- neutralize pause/resume, keeping the raw event ------------------------------------------
# From here on, always restore the crate on exit.
trap restore EXIT

echo
echo "[ab] neutralizing pause/resume in $FILE (raw event kept; auto-reverted on exit)"
# Content-matched edits (robust to line drift):
#   1. constructor: start the counter ENABLED (was `counter.disable()?;`) so a no-op resume
#      doesn't leave it reading 0 forever.
#   2/3. LinuxBranchCounter::pause/resume bodies -> no-ops (the counter free-runs across the
#      host dispatch between exits, i.e. pre-bracket behavior).
sed -i 's#^        counter.disable()?;#        counter.enable()?;#'                    "$FILE"
sed -i 's#^        let _ = self.counter.disable();#        /* ab: pause neutralized */#' "$FILE"
sed -i 's#^        let _ = self.counter.enable();#        /* ab: resume neutralized */#'  "$FILE"

# Verify all three edits actually applied; if the source drifted, abort (trap reverts).
if ! grep -q 'counter.enable()?;' "$FILE" \
   || grep -q 'let _ = self.counter.disable();' "$FILE" \
   || grep -q 'let _ = self.counter.enable();'  "$FILE"; then
    echo "ERROR: neutralize did not apply cleanly (source drifted); reverting, no Arm B." >&2
    exit 1
fi
git --no-pager diff --stat -- "$FILE"

# --- Arm B: neutralized (raw event, NO pause/resume) -----------------------------------------
run_arm "B (raw event, NO pause/resume)" "$LOGB"

# revert immediately (trap will also run, harmlessly)
restore
trap - EXIT

# --- results ---------------------------------------------------------------------------------
PA="$(pass_count "$LOGA")"; FA="$(fail_count "$LOGA")"
PB="$(pass_count "$LOGB")"; FB="$(fail_count "$LOGB")"
PA=${PA:-0}; FA=${FA:-0}; PB=${PB:-0}; FB=${FB:-0}

echo
echo "================== A/B RESULT (N=$N) =================="
printf "  Arm A  raw event + pause/resume :  PASS=%s  FAIL=%s\n" "$PA" "$FA"
printf "  Arm B  raw event, NO pause/resume:  PASS=%s  FAIL=%s\n" "$PB" "$FB"
echo   "------------------------------------------------------"

incomplete=0
[[ $((PA + FA)) -eq "$N" ]] || { echo "  [warn] Arm A logged $((PA+FA))/$N runs — drive script may have aborted early."; incomplete=1; }
[[ $((PB + FB)) -eq "$N" ]] || { echo "  [warn] Arm B logged $((PB+FB))/$N runs — drive script may have aborted early."; incomplete=1; }

if [[ "$incomplete" -eq 1 ]]; then
    echo "  VERDICT: INCONCLUSIVE — a run didn't complete N boots; inspect the logs:"
    echo "           Arm A: $LOGA"
    echo "           Arm B: $LOGB"
    exit 0
fi

DELTA=$((PA - PB))
if [[ "$PA" -lt $((N - 1)) ]]; then
    echo "  VERDICT: INCONCLUSIVE — Arm A itself is flaky ($PA/$N); the raw-event baseline"
    echo "           should be ~$N/$N. Investigate before judging pause/resume."
elif [[ "$DELTA" -le 1 ]]; then
    echo "  VERDICT: REDUNDANT — Arm B matches Arm A within noise (delta=$DELTA)."
    echo "           The raw 0x11c4 event subsumes what pause/resume bought; the bracketing"
    echo "           can be simplified/removed. (Bump N=50 to tighten if delta=1.)"
else
    echo "  VERDICT: LOAD-BEARING — removing pause/resume dropped PASS by $DELTA (=$PA -> $PB)."
    echo "           The bracketing does real work even with the raw event; keep it."
fi
echo "  logs: Arm A=$LOGA  Arm B=$LOGB"
echo "======================================================"
