#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/gate.sh — the standard verification gate, parallelized.
#
# Replaces the hand-run sequence
#     cargo build --workspace && cargo clippy --workspace --all-targets && cargo test --workspace
#     && bash drive/h/h0.sh && ... && bash drive/pkg/pkg-virtio-rng-generate-cli.sh
# with a phased runner that overlaps what can overlap and skips what provably cannot
# have changed.
#
#   ./drive/gate.sh                  # full gate
#   ./drive/gate.sh --jobs 1         # identical work, strictly serial — the A/B baseline
#   ./drive/gate.sh --no-h5-first    # queue h5 inside the pool instead of running it alone
#   ./drive/gate.sh --skip-cargo     # drive scripts only
#   ./drive/gate.sh --force-build-cli
#   ./drive/gate.sh --no-flake-rerun # skip the phase-6 isolation re-run (see below)
#
# WHY THE PHASES ARE WHAT THEY ARE — each of these is load-bearing:
#
#   Phase 0 warm-up. The 21 drive scripts issue 35 cargo calls between them, all sharing one
#   target-dir lock, and a no-op cargo call costs ~6.7s here (drvfs stat latency over ~9k dep
#   files). Building once up front and exporting BAUD_GATE_PREBUILT lets the scripts skip their
#   own `cargo build`, removing ~2 minutes of pure serialized overhead.
#
#   Phase 3 fan-out is safe only because the scripts were fixed to (a) not `pkill -f baud-server`
#   — which also matched sibling `cargo build -p baud-server` and its rustc, so one script's
#   startup killed another script's *build*; (b) bind an ephemeral port via BAUD_ADDR instead of
#   a hardcoded 7734; (c) health-poll instead of `sleep 1`, which previously let a script whose
#   own server lost the bind silently drive somebody else's server and pass; (d) keep snapshot
#   stores and render outputs in per-run temp dirs.
#
#   Fan-out is measurably real, not theoretical: `cargo test` releases the target-dir lock before
#   the tests EXECUTE. Measured on this host — a `cargo build` finished in 7.2s while a 243.8s
#   test was mid-run. If cargo had held the lock through execution, the nine scripts that run
#   `cargo test` would have serialized regardless of this orchestrator.
#
#   Phase 4 h6 alone. fleet_of_vms_run_in_parallel_without_interference times a serial baseline
#   and asserts parallel_total < serial_one * n * 0.85, and run_fleet pins to fixed cores 0/2/4.
#   Concurrent load skews both terms and fails it legitimately. It gets the host to itself.
#
#   Phase 5 gated. pkg-build-cli.sh is ~4-5 min, of which only ~65s is the kernel compile; the
#   rest is `cp -a` of a 1.8G tree plus `mrproper`, uncached, every run. Its dominant input
#   (~/wsl-kernel-src/src) lives outside git and is mutated in place by drive/manual/h3-enforced-*.sh,
#   so a git-diff rule alone would be unsound — the stamp fingerprints the tree state too.
#
#   Phase 6 flake isolation. rdtsc_guest_reproduces_high_bits_across_boots is the one test in
#   this gate with both a documented load-flake history AND a known mechanical cause:
#   KVM_SET_MSRS(IA32_TSC=0) pins only the counter's STARTING value, so real host-scheduling
#   jitter between that ioctl and the guest's first rdtsc perturbs bits below RDTSC_JITTER_MASK's
#   20-bit tolerance (crates/baud-multiverse/tests/fixtures/rdtsc-guest/BUILD.md). An 8-wide
#   fan-out is precisely the load that produces that jitter, and the test reaches the pool from
#   two directions at once — the workspace `cargo test` and h3.sh's own H3.4 step — so a failure
#   here says nothing until the test has been given an idle host. Phase 6 therefore re-runs just
#   that test, alone, after every other unit has drained, and reports BOTH results.
#
#   Passing in isolation does NOT turn the gate green. todo.md's standing rule is "report a flake
#   as a flake, with both results; a failure that reproduces in isolation is real and must not be
#   worked around" — a gate that excuses its own failures stops being evidence. A flake is
#   reclassified FAIL -> FLAKE so it reads as distinct from a regression, and the gate still
#   exits 1. The operator decides.
#
# Enforced-regime scripts (h3-enforced-*, h7-enforced-*) are deliberately NOT in this gate: they
# rmmod/insmod the live kvm_intel and guard on `fuser /dev/kvm`, making them mutually exclusive
# with every other baud process on the box. Run those by hand, one at a time.

set -uo pipefail

# Job control, so every unit we launch with `&` becomes its own PROCESS GROUP leader
# (pgid == pid). That is what lets an interrupted gate reap each unit's entire subtree
# — the drive script, the baud-server it spawned, any cargo/rustc under it — with a
# single `kill -- -$pid`, and touch nothing else.
#
# Deliberately NOT `pkill -f baud-server` or any other pattern match: that is what used
# to make these scripts kill each other's servers AND each other's `cargo build`, and it
# would also kill a completely unrelated baud invocation the user happens to be running.
# We only ever signal process groups we started ourselves.
set -m

cd "$(dirname "$0")/.."
REPO_ROOT="$(pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

JOBS=8
H5_FIRST=1
SKIP_CARGO=0
FORCE_BUILD_CLI=0
FLAKE_RERUN=1
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)"
LOGDIR="${BAUD_GATE_LOGDIR:-$REPO_ROOT/target/gate-logs/$RUN_ID}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        -j|--jobs)          JOBS="$2"; shift 2 ;;
        --h5-first)         H5_FIRST=1; shift ;;
        --no-h5-first)      H5_FIRST=0; shift ;;
        --skip-cargo)       SKIP_CARGO=1; shift ;;
        --force-build-cli)  FORCE_BUILD_CLI=1; shift ;;
        --flake-rerun)      FLAKE_RERUN=1; shift ;;
        --no-flake-rerun)   FLAKE_RERUN=0; shift ;;
        --logdir)           LOGDIR="$2"; shift 2 ;;
        -h|--help)          sed -n '2,64p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) echo "gate: unknown option $1 (try --help)" >&2; exit 2 ;;
    esac
done

mkdir -p "$LOGDIR"

BOLD=$'\033[1m'; RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'; RESET=$'\033[0m'
[[ -t 1 ]] || { BOLD=""; RED=""; GREEN=""; YELLOW=""; RESET=""; }

GATE_START=$SECONDS
RESULTS_FILE="$LOGDIR/results.tsv"
: > "$RESULTS_FILE"

say() { printf '%s[gate %s]%s %s\n' "$BOLD" "$(date -u +%H:%M:%S)" "$RESET" "$*"; }

# One row per unit: name, status, seconds, log. Written from background subshells
# too, so every write must be a single short line (atomic under O_APPEND).
record() { printf '%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" >> "$RESULTS_FILE"; }

# Runs one unit, times it, captures output to its own log. Deliberately never
# propagates failure: one broken script must not abort the rest of the gate, or a
# single failure hides the state of everything after it. The tally sets exit code.
run_unit() { # <name> <command...>
    local name="$1"; shift
    local log="$LOGDIR/$name.log"
    local t0=$SECONDS rc=0
    "$@" > "$log" 2>&1 || rc=$?
    local secs=$((SECONDS - t0))
    if (( rc == 0 )); then
        record "$name" PASS "$secs" "$log"
        printf '  %sPASS%s %-38s %4ds\n' "$GREEN" "$RESET" "$name" "$secs"
    else
        record "$name" FAIL "$secs" "$log"
        printf '  %sFAIL%s %-38s %4ds  rc=%d  %s\n' "$RED" "$RESET" "$name" "$secs" "$rc" "$log"
    fi
}

# Every in-flight unit, by pid. Thanks to `set -m` each is also a process-group id, so
# signalling -PID reaches the unit's whole subtree and nothing outside it.
ACTIVE=()

prune_active() { # drop pids that have exited
    local p keep=()
    for p in "${ACTIVE[@]:-}"; do [[ -n "$p" ]] && kill -0 "$p" 2>/dev/null && keep+=("$p"); done
    ACTIVE=("${keep[@]:-}")
}

# Reap every unit we started, and only those. TERM first so each drive script's own
# `trap 'exit 143' TERM` fires and its cleanup() removes its server, temp DB and
# snapshot dir; KILL only for anything that ignores it. Without the TERM-first step
# an interrupted gate strands baud-servers holding /dev/kvm and leaves temp files
# behind, which then corrupts every later measurement.
reap_units() {
    local p alive
    for p in "${ACTIVE[@]:-}"; do
        [[ -n "$p" ]] || continue
        kill -TERM -- "-$p" 2>/dev/null || kill -TERM "$p" 2>/dev/null || true
    done

    # Wait for each process GROUP to drain, not just its leader. The leader here is the
    # run_unit subshell, which dies on TERM instantly — polling `kill -0 $leader` therefore
    # returns "gone" while the drive script underneath is still inside cleanup(), and
    # escalating to KILL at that moment is exactly what strands baud-servers holding
    # /dev/kvm and leaves temp SQLite files and snapshot dirs behind. `pgrep -g` asks the
    # real question: is anything at all still alive in this group?
    for _ in $(seq 1 60); do
        alive=0
        for p in "${ACTIVE[@]:-}"; do
            [[ -n "$p" ]] && [[ -n "$(pgrep -g "$p" 2>/dev/null)" ]] && alive=1
        done
        (( alive )) || break
        sleep 0.5
    done

    for p in "${ACTIVE[@]:-}"; do
        [[ -n "$p" ]] || continue
        kill -KILL -- "-$p" 2>/dev/null || kill -KILL "$p" 2>/dev/null || true
    done
    ACTIVE=()

    # A KILLed guest's fds are closed by the kernel asynchronously; give /dev/kvm a moment
    # to actually come free so the next run does not start against a still-busy device.
    for _ in $(seq 1 20); do
        fuser /dev/kvm >/dev/null 2>&1 || break
        sleep 0.5
    done
}

# Who held /dev/kvm before we started. Anything in this set is somebody else's — a
# developer's test binary, another tool — and must never be reported as our leftover
# (nor, of course, killed).
KVM_HOLDERS_AT_START="$(fuser /dev/kvm 2>/dev/null | tr -s ' ' '\n' | grep -E '^[0-9]+$' | sort -u | tr '\n' ' ')"

kvm_holders_we_left() {
    local now pid out=""
    now="$(fuser /dev/kvm 2>/dev/null | tr -s ' ' '\n' | grep -E '^[0-9]+$' | sort -u)"
    for pid in $now; do
        case " $KVM_HOLDERS_AT_START " in *" $pid "*) ;; *) out="$out $pid" ;; esac
    done
    printf '%s' "${out# }"
}

INTERRUPTED=0
on_signal() { # <exit-code> <label>
    (( INTERRUPTED )) && return
    INTERRUPTED=1
    echo ""
    say "${RED}interrupted ($2) — reaping ${#ACTIVE[@]} in-flight unit(s)${RESET}"
    reap_units
    local left; left="$(kvm_holders_we_left)"
    if [[ -z "$left" ]]; then
        say "reaped cleanly — no /dev/kvm holders left by this gate${KVM_HOLDERS_AT_START:+ (pre-existing, untouched: ${KVM_HOLDERS_AT_START% })}"
    else
        say "${RED}WARNING: this gate left /dev/kvm holders: $left${RESET}"
    fi
    exit "$1"
}
trap 'on_signal 130 SIGINT'  INT
trap 'on_signal 143 SIGTERM' TERM
trap 'reap_units' EXIT

pool_run() { # <name> <command...>
    while (( $(jobs -rp | wc -l) >= JOBS )); do wait -n 2>/dev/null || true; done
    run_unit "$@" &
    ACTIVE+=($!)
}
pool_drain() {
    local p
    for p in "${ACTIVE[@]:-}"; do [[ -n "$p" ]] && wait "$p" 2>/dev/null; done
    ACTIVE=()
}

# Serial units go through the same machinery, so an interrupt during phase 0/1/2/4/5
# reaps them exactly like a fan-out unit. Running them in the foreground instead would
# leave their children sharing this shell's process group, where they could not be
# signalled without killing the gate itself.
run_one() { # <name> <command...>
    run_unit "$@" &
    local p=$!
    ACTIVE+=("$p")
    wait "$p" 2>/dev/null
    prune_active
}

failed_count() { grep -c $'\tFAIL\t' "$RESULTS_FILE" 2>/dev/null || true; }

# ── documented-flake machinery (phase 6) ─────────────────────────────────────

FLAKE_TEST="rdtsc_guest_reproduces_high_bits_across_boots"
ISO_LOG=""; ISO_SECS=0; ISO_RC=0

# Was $FLAKE_TEST the SOLE reason this unit failed? Downgrading a unit that failed for
# this test AND something else would hide the something else, so this must be exact.
#
# libtest prints exactly one `---- <name> stdout ----` block per FAILING test (passing
# tests get no block), so comparing "blocks naming our test" against "blocks in total"
# separates "only this test flaked" from "this test flaked and three others really
# broke". h3.sh is the other route in: it runs the test as its H3.4 step and its fail()
# exits on the spot, so its marker line is conclusive by itself.
flake_is_sole_cause() { # <log>
    local log="$1" total mine
    [[ -r "$log" ]] || return 1
    grep -q "$FLAKE_TEST FAILED" "$log" 2>/dev/null && return 0
    total=$(grep -cE '^---- .* stdout ----' "$log" 2>/dev/null || true)
    mine=$(grep -cE "^---- .*${FLAKE_TEST} stdout ----" "$log" 2>/dev/null || true)
    (( ${mine:-0} > 0 && ${mine:-0} == ${total:-0} ))
}

# FAIL -> FLAKE for one unit. Safe to rewrite the file wholesale here because phase 6
# runs after every unit has drained, so nothing else is appending to it.
mark_flake() { # <unit-name>
    local n="$1" tmp="$RESULTS_FILE.tmp"
    awk -F'\t' -v OFS='\t' -v n="$n" '$1==n && $2=="FAIL" { $2="FLAKE" } 1' \
        "$RESULTS_FILE" > "$tmp" && mv "$tmp" "$RESULTS_FILE"
}

# Like run_one — same process-group reaping — but reports rc in ISO_RC instead of
# recording a results row, since the isolation re-run is evidence about an existing
# row, not a unit of its own.
run_isolated() { # <name> <command...>
    local name="$1"; shift
    local t0=$SECONDS
    ISO_LOG="$LOGDIR/$name.log"
    ISO_RC=0
    "$@" > "$ISO_LOG" 2>&1 &
    local p=$!
    ACTIVE+=("$p")
    wait "$p" 2>/dev/null || ISO_RC=$?
    prune_active
    ISO_SECS=$((SECONDS - t0))
}

# ── phase 0: warm-up ─────────────────────────────────────────────────────────

if (( ! SKIP_CARGO )); then
    say "phase 0: warm-up build (primes the shared target dir for every later cargo call)"
    run_one "00-warmup-build" cargo build --workspace --tests --bins

    # A failed warm-up would make every downstream unit fail on the same compile
    # error, burying the real message in 21 identical logs. Stop and show it.
    if (( $(failed_count) > 0 )); then
        say "${RED}warm-up build failed — aborting before fan-out${RESET}"
        tail -40 "$LOGDIR/00-warmup-build.log"
        exit 1
    fi

    # Clippy and the workspace tests are queued as ordinary pool units rather than run
    # ahead of the fan-out. Nothing in phase 3 depends on either, and `cargo test`
    # releases the target-dir lock before the tests EXECUTE (measured: a `cargo build`
    # finished in 7.2s while a 243.8s test was mid-run), so they overlap the drive
    # scripts instead of adding their ~75s to the critical path. Queued FIRST so the
    # longest-running units all start early.
    say "phase 1+2: clippy and workspace tests (queued into the fan-out pool)"
    QUEUE_CARGO=1
fi

# Every drive script skips its own `cargo build` from here on. Set only after the
# warm-up actually succeeded — otherwise the scripts would run against stale or
# missing binaries and fail in ways that look like real regressions.
export BAUD_GATE_PREBUILT=1

# ── phase 3: drive-script fan-out ────────────────────────────────────────────
# Paths are relative to drive/; the unit's log name is the basename, so a log stays
# `03-h5.log` rather than picking up the directory.

FANOUT=(
    h/h7 pkg/pkg-multifile-initramfs pkg/pkg-dynamic-link h/h2 h/h3
    m/m9 pkg/pkg-virtio-rng-branch-resume-cli pkg/pkg-boot-cli h/h4
    m/m10 m/m11 m/m12 m/m13 pkg/pkg-virtio-rng-replay-cli pkg/pkg-virtio-rng-generate-cli
    h/h1 h/h0 pkg/pkg-virtio-rng-cli
)

# h5 runs ALONE first, ahead of everything else, and this is the default.
#
# It is the long pole, and since `thousand_branches_are_independent_and_deterministic`
# now parallelises its own 1000 branches across BRANCH_WORKERS threads, h5 already
# saturates the box by itself — running anything alongside it oversubscribes the cores
# and inflates the very unit that sets the floor (measured: ~88s alone, ~126s sharing
# the host at --jobs 2). Isolating it costs the overlap of the other ~140s of scripts,
# but they fan out N-wide immediately afterwards and drain far faster than h5 slows down.
# --no-h5-first restores the old behaviour (h5 queued first inside the pool) for A/B.
if (( H5_FIRST )); then
    say "phase 3a: h5 alone (long pole, uncontended — it is internally multi-threaded)"
    run_one "03-h5" bash drive/h/h5.sh
else
    FANOUT=(h/h5 "${FANOUT[@]}")
fi

say "phase 3: drive scripts (${JOBS}-wide)"
# Order matters: the longest remaining unit must start first, or it finishes last by
# exactly the delay. The cargo units go next (they are the next-longest), then the
# short scripts.
FIRST_UNIT="${FANOUT[0]:-}"
[[ -n "$FIRST_UNIT" ]] && pool_run "03-${FIRST_UNIT##*/}" bash "drive/$FIRST_UNIT.sh"
if (( ${QUEUE_CARGO:-0} )); then
    pool_run "02-cargo-test" cargo test --workspace
    pool_run "01-clippy" cargo clippy --workspace --all-targets
fi
for s in "${FANOUT[@]:1}"; do
    [[ -n "$s" ]] || continue
    pool_run "03-${s##*/}" bash "drive/$s.sh"
done
pool_drain

# ── phase 4: exclusive ───────────────────────────────────────────────────────

say "phase 4: h6 (exclusive — its speedup assertion needs an otherwise-idle host)"
run_one "04-h6" bash drive/h/h6.sh

# ── phase 5: gated kernel-build pipeline ─────────────────────────────────────

STAMP="drive/pkg/pkg-build-cli.stamp"
KSRC="${BAUD_KERNEL_SRC:-$HOME/wsl-kernel-src/src}"

# Fingerprints every input that can change the outcome, including the two no
# git-diff rule can see: the out-of-tree kernel source, and whether the
# enforced-regime RDTSC patch is currently applied to it.
build_cli_fingerprint() {
    {
        sha256sum crates/baud-multiverse/tests/fixtures/linux-guest/minimal.config \
                  crates/baud-multiverse/tests/fixtures/linux-guest/init.c \
                  drive/pkg/pkg-build-cli.sh 2>/dev/null
        find crates/baud-packages/src -name '*.rs' -exec sha256sum {} + 2>/dev/null | sort
        sha256sum crates/baud-server/src/routes/image.rs \
                  crates/baud-cli/src/cmds/image.rs \
                  Cargo.lock 2>/dev/null
        make -s -C "$KSRC" kernelversion 2>/dev/null || echo "NO-KERNEL-TREE"
        grep -c handle_baud_rdtsc_exit "$KSRC/arch/x86/kvm/vmx/vmx.c" 2>/dev/null || echo "NO-VMX"
        gcc-13 --version 2>/dev/null | head -1 || echo "NO-GCC13"
    } | sha256sum | cut -d' ' -f1
}

FP="$(build_cli_fingerprint)"
PREV="$(cat "$STAMP" 2>/dev/null || echo "")"

# Fail-safe in all directions: a missing stamp, an unreadable kernel tree, or an
# unrecognized toolchain all hash differently and therefore RUN the script.
if (( FORCE_BUILD_CLI )) || [[ "$FP" != "$PREV" ]]; then
    if (( FORCE_BUILD_CLI )); then
        say "phase 5: pkg-build-cli (forced, ~4-5 min)"
    else
        say "phase 5: pkg-build-cli (inputs changed since last pass, ~4-5 min)"
    fi
    run_one "05-pkg-build-cli" bash drive/pkg/pkg-build-cli.sh
    grep -q $'^05-pkg-build-cli\tPASS\t' "$RESULTS_FILE" && printf '%s\n' "$FP" > "$STAMP"
else
    say "phase 5: pkg-build-cli ${YELLOW}SKIPPED${RESET} — no input changed since the last pass"
    record "05-pkg-build-cli" SKIP 0 "-"
fi

# ── phase 6: documented-flake isolation re-run ───────────────────────────────
# Last, so the host is idle: h5, the fan-out, h6 and pkg-build-cli have all drained.
# See the header for why this test specifically, and why passing here does not turn
# the gate green.

FLAKE_CANDIDATES=()
if (( FLAKE_RERUN )) && (( $(failed_count) > 0 )); then
    while IFS=$'\t' read -r fname fstatus _ flog; do
        [[ "$fstatus" == "FAIL" ]] || continue
        flake_is_sole_cause "$flog" && FLAKE_CANDIDATES+=("$fname")
    done < "$RESULTS_FILE"
fi

if (( ${#FLAKE_CANDIDATES[@]} > 0 )); then
    say "phase 6: $FLAKE_TEST is the sole cause of ${#FLAKE_CANDIDATES[@]} failing unit(s) (${FLAKE_CANDIDATES[*]})"
    say "phase 6: re-running it alone on the now-idle host"
    run_isolated "06-flake-rdtsc-isolated" \
        cargo test -q -p baud-multiverse "$FLAKE_TEST" -- --test-threads=1
    if (( ISO_RC == 0 )); then
        say "${YELLOW}phase 6: PASSED in isolation (${ISO_SECS}s) — documented load-flake, not a regression${RESET}"
        for fname in "${FLAKE_CANDIDATES[@]}"; do mark_flake "$fname"; done
    else
        say "${RED}phase 6: FAILED in isolation too (${ISO_SECS}s, rc=$ISO_RC) — this is a real regression${RESET}"
    fi
fi

# ── tally ────────────────────────────────────────────────────────────────────

TOTAL=$((SECONDS - GATE_START))
PASS_N=$(grep -c $'\tPASS\t' "$RESULTS_FILE" || true)
FAIL_N=$(failed_count)
FLAKE_N=$(grep -c $'\tFLAKE\t' "$RESULTS_FILE" || true)
SKIP_N=$(grep -c $'\tSKIP\t' "$RESULTS_FILE" || true)

echo ""
say "──────────────────────────────────────────────"
printf '  %sslowest units%s\n' "$BOLD" "$RESET"
sort -t$'\t' -k3 -rn "$RESULTS_FILE" | head -5 | while IFS=$'\t' read -r n s d _; do
    printf '    %-38s %4ds  %s\n' "$n" "$d" "$s"
done
echo ""
# "flaked" only appears when there is one, so a clean run's summary line is unchanged.
FLAKE_TXT=""
(( ${FLAKE_N:-0} > 0 )) && FLAKE_TXT="$(printf '%d flaked, ' "${FLAKE_N:-0}")"
printf '  %d passed, %d failed, %s%d skipped in %dm%02ds (jobs=%d)\n' \
       "${PASS_N:-0}" "${FAIL_N:-0}" "$FLAKE_TXT" "${SKIP_N:-0}" $((TOTAL / 60)) $((TOTAL % 60)) "$JOBS"
printf '  logs: %s\n' "$LOGDIR"

if (( ${FLAKE_N:-0} > 0 )); then
    echo ""
    printf '  %sflakes:%s\n' "$YELLOW" "$RESET"
    grep $'\tFLAKE\t' "$RESULTS_FILE" | while IFS=$'\t' read -r n _ d l; do
        printf '    %-38s %4ds  %s\n' "$n" "$d" "$l"
        printf '      %s\n' "$FLAKE_TEST"
        printf '      FAILED under %d-wide fan-out, PASSED in isolation (%ds)  %s\n' \
               "$JOBS" "$ISO_SECS" "$ISO_LOG"
        printf '      documented load-flake, not a regression — see todo.md and\n'
        printf '      crates/baud-multiverse/tests/fixtures/rdtsc-guest/BUILD.md\n'
    done
fi

if (( ${FAIL_N:-0} > 0 )); then
    echo ""
    printf '  %sfailures:%s\n' "$RED" "$RESET"
    grep $'\tFAIL\t' "$RESULTS_FILE" | while IFS=$'\t' read -r n _ d l; do
        printf '    %-38s %4ds  %s\n' "$n" "$d" "$l"
    done
    echo ""
    printf '  NOTE: this host has a documented load-flake history in\n'
    printf '        timer_tick_lands_at_identical_instruction, rdtsc_guest_reproduces_high_bits_across_boots,\n'
    printf '        fleet_of_vms_run_in_parallel_without_interference, and `baud host probe` regime=rejected.\n'
    printf '        Re-run a failing unit in isolation before treating it as a regression.\n'
    printf '        (rdtsc_guest_reproduces_high_bits_across_boots is re-run automatically by phase 6\n'
    printf '        when it is the sole cause of a unit failure; the other three are still by hand.)\n'
    exit 1
fi

# Reported, not excused: a flake still exits 1. See the header.
if (( ${FLAKE_N:-0} > 0 )); then
    echo ""
    say "${YELLOW}gate not green — no regressions found, but ${FLAKE_N} unit(s) flaked (see above)${RESET}"
    exit 1
fi

say "${GREEN}gate green${RESET}"
exit 0
