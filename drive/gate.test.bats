#!/usr/bin/env bats
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/gate.test.bats — regression tests for drive/gate.sh and the concurrency-safety
# contract the drive scripts must uphold.
#
#   bats drive/gate.test.bats                      # everything (slow tests boot guests)
#   bats drive/gate.test.bats --filter-tags '!slow'  # static checks only, ~1s
#
# Tests tagged `slow` spawn real servers and drive scripts and must have the machine to
# themselves — they interrupt a running gate on purpose. Do not run them alongside other
# KVM work or their assertions about leftover processes become meaningless.
#
# WHY THIS FILE EXISTS. Every invariant below was violated at some point and cost real
# debugging time:
#   - `pkill -f baud-server` matched sibling `cargo build -p baud-server` and its rustc, so
#     one script's startup killed another script's BUILD.
#   - A hardcoded 127.0.0.1:7734 plus a bare `sleep 1` meant a script whose own server lost
#     the bind silently drove somebody else's server and PASSED.
#   - `trap cleanup EXIT` alone does not run on an untrapped signal, so an interrupted gate
#     stranded baud-servers holding /dev/kvm and leaked 21 temp SQLite files.
#   - Polling `kill -0` on a process-GROUP LEADER reports "gone" while the drive script
#     underneath is still inside cleanup(), so escalating to KILL there re-created the leak.

setup_file() {
    cd "$BATS_TEST_DIRNAME/.." || exit 1
    export REPO_ROOT="$PWD"
}

setup() {
    cd "$REPO_ROOT" || exit 1
}

# The scripts that spawn a baud-server and therefore must be concurrency-safe.
server_scripts() {
    echo drive/h/h0.sh drive/h/h1.sh drive/h/h2.sh drive/h/h3.sh drive/h/h4.sh drive/h/h5.sh \
         drive/h/h6.sh drive/h/h7.sh drive/m/m9.sh drive/m/m10.sh drive/m/m11.sh drive/m/m12.sh \
         drive/m/m13.sh drive/pkg/pkg-boot-cli.sh drive/pkg/pkg-virtio-rng-cli.sh \
         drive/pkg/pkg-virtio-rng-replay-cli.sh drive/pkg/pkg-virtio-rng-branch-resume-cli.sh \
         drive/pkg/pkg-virtio-rng-generate-cli.sh
}

kvm_holders() {
    fuser /dev/kvm 2>/dev/null | tr -s ' ' '\n' | grep -E '^[0-9]+$' | sort -u | tr '\n' ' '
}

# ── CLI surface ──────────────────────────────────────────────────────────────

@test "gate --help exits 0 and describes usage" {
    run ./drive/gate.sh --help
    [ "$status" -eq 0 ]
    [[ "$output" == *"--jobs"* ]]
}

@test "gate rejects an unknown option instead of silently ignoring it" {
    run ./drive/gate.sh --definitely-not-an-option
    [ "$status" -eq 2 ]
}

@test "gate is syntactically valid bash" {
    run bash -n ./drive/gate.sh
    [ "$status" -eq 0 ]
}

# ── no pattern-based process killing anywhere ────────────────────────────────

@test "gate never uses pkill/killall (would hit unrelated baud invocations)" {
    # Comments may mention pkill; actual invocations must not exist.
    run bash -c "grep -nE '^[^#]*\\b(pkill|killall)\\b' drive/gate.sh"
    [ "$status" -ne 0 ]
}

@test "no gate-scope drive script invokes pkill" {
    for s in $(server_scripts) drive/pkg/pkg-build-cli.sh; do
        run bash -c "grep -nE '^[^#]*\\bpkill\\b' '$s'"
        [ "$status" -ne 0 ] || { echo "pkill still present in $s: $output"; false; }
    done
}

# ── signal handling: cleanup must run on interrupt ───────────────────────────

@test "every server-spawning script traps INT and TERM, not just EXIT" {
    # bash does NOT run an EXIT trap when killed by an untrapped signal, so a plain
    # `trap cleanup EXIT` leaks the server, temp DB and snapshot dir on interrupt.
    for s in $(server_scripts); do
        run grep -q "trap 'exit 143' TERM" "$s"
        [ "$status" -eq 0 ] || { echo "$s has no TERM trap"; false; }
        run grep -q "trap 'exit 130' INT" "$s"
        [ "$status" -eq 0 ] || { echo "$s has no INT trap"; false; }
    done
}

@test "gate enables job control so each unit is its own process group" {
    run grep -qE '^set -m' drive/gate.sh
    [ "$status" -eq 0 ]
}

@test "gate waits on the process GROUP, not just the leader, before escalating to KILL" {
    # The leader is the run_unit subshell and dies on TERM immediately; polling it
    # reports "gone" while the drive script is still inside cleanup().
    run grep -q 'pgrep -g' drive/gate.sh
    [ "$status" -eq 0 ]
}

@test "gate reaps on INT, TERM and EXIT" {
    run grep -q "trap 'on_signal 130 SIGINT'  INT" drive/gate.sh
    [ "$status" -eq 0 ]
    run grep -q "trap 'on_signal 143 SIGTERM' TERM" drive/gate.sh
    [ "$status" -eq 0 ]
    run grep -q "trap 'reap_units' EXIT" drive/gate.sh
    [ "$status" -eq 0 ]
}

@test "serial units go through run_one so they are reapable too" {
    # In the foreground they would share the gate's own process group and could not be
    # signalled without killing the gate itself.
    for unit in 00-warmup-build 04-h6 05-pkg-build-cli; do
        run grep -q "run_one \"$unit\"" drive/gate.sh
        [ "$status" -eq 0 ] || { echo "$unit is not run via run_one"; false; }
    done
}

@test "clippy and workspace tests overlap the fan-out instead of blocking it" {
    # Nothing in phase 3 depends on either, and `cargo test` releases the target-dir lock
    # before executing, so running them ahead of the fan-out just adds ~75s to the
    # critical path. They are pool units (still reapable — pool_run tracks them in ACTIVE).
    for unit in 01-clippy 02-cargo-test; do
        run grep -q "pool_run \"$unit\"" drive/gate.sh
        [ "$status" -eq 0 ] || { echo "$unit does not overlap the fan-out"; false; }
    done
}

@test "the longest unit is queued before the cargo units" {
    # Whatever starts last finishes last by exactly its delay; the long pole must go first.
    h5_line=$(grep -n 'run_one "03-h5"' drive/gate.sh | head -1 | cut -d: -f1)
    cargo_line=$(grep -n 'pool_run "02-cargo-test"' drive/gate.sh | head -1 | cut -d: -f1)
    [ -n "$h5_line" ] && [ -n "$cargo_line" ] && [ "$h5_line" -lt "$cargo_line" ]
}

@test "gate distinguishes pre-existing /dev/kvm holders from its own leftovers" {
    run grep -q 'KVM_HOLDERS_AT_START' drive/gate.sh
    [ "$status" -eq 0 ]
}

# ── port isolation ───────────────────────────────────────────────────────────

@test "no gate-scope script hardcodes 127.0.0.1:7734" {
    for s in $(server_scripts); do
        run bash -c "grep -n '127\\.0\\.0\\.1:7734' '$s'"
        [ "$status" -ne 0 ] || { echo "$s still hardcodes 7734: $output"; false; }
    done
}

@test "every server-spawning script binds an ephemeral port via BAUD_ADDR" {
    for s in $(server_scripts); do
        run grep -q 'BAUD_ADDR="127.0.0.1:$BAUD_PORT"' "$s"
        [ "$status" -eq 0 ] || { echo "$s does not pass BAUD_ADDR"; false; }
        run grep -q 'export BAUD_SERVER=' "$s"
        [ "$status" -eq 0 ] || { echo "$s does not export BAUD_SERVER for the CLI"; false; }
    done
}

@test "baud-server still defaults to 127.0.0.1:7734 when BAUD_ADDR is unset" {
    # Backwards compatibility: every existing workflow must be unaffected.
    run grep -A2 'std::env::var("BAUD_ADDR")' crates/baud-server/src/main.rs
    [ "$status" -eq 0 ]
    [[ "$output" == *'127.0.0.1:7734'* ]]
}

@test "scripts health-poll the server instead of sleeping a fixed second" {
    # A bare `sleep 1` let a script whose own server lost the bind drive a foreign
    # server and report a false pass.
    for s in $(server_scripts); do
        run grep -q '/health' "$s"
        [ "$status" -eq 0 ] || { echo "$s does not health-poll"; false; }
    done
}

# ── prebuilt guard ───────────────────────────────────────────────────────────

@test "scripts guard cargo build behind BAUD_GATE_PREBUILT but never guard cargo test" {
    for s in $(server_scripts); do
        run grep -q 'BAUD_GATE_PREBUILT' "$s"
        [ "$status" -eq 0 ] || { echo "$s lacks the prebuilt guard"; false; }
    done
    # Guarding a `cargo test` would silently skip the assertions the script exists for,
    # turning a skipped build into a skipped TEST. Check the guarded BLOCKS structurally
    # (if ... fi), not a fixed-size context window — a nearby unguarded `cargo test` on the
    # line after `fi` is correct and must not trip this.
    for s in $(server_scripts); do
        run awk '
            /if \[\[ -z "\$\{BAUD_GATE_PREBUILT:-\}" \]\]; then/ { depth = 1; next }
            depth && /^[[:space:]]*fi[[:space:]]*$/            { depth = 0; next }
            depth && /cargo[[:space:]]+test/                   { print FILENAME ": " $0; found = 1 }
            END { exit(found ? 1 : 0) }
        ' "$s"
        [ "$status" -eq 0 ] || { echo "cargo test is inside a prebuilt guard in $s: $output"; false; }
    done
}

@test "gate exports BAUD_GATE_PREBUILT only after the warm-up build succeeded" {
    # Exporting it after a failed warm-up would run every script against stale binaries.
    warm=$(grep -n 'abort.*fan-out\|aborting before fan-out' drive/gate.sh | head -1 | cut -d: -f1)
    exp=$(grep -n '^export BAUD_GATE_PREBUILT=1' drive/gate.sh | head -1 | cut -d: -f1)
    [ -n "$warm" ] && [ -n "$exp" ] && [ "$exp" -gt "$warm" ]
}

# ── scheduling invariants ────────────────────────────────────────────────────

@test "h6 is excluded from the fan-out pool and run exclusively" {
    # fleet_of_vms_run_in_parallel_without_interference asserts a speedup RATIO against a
    # serial baseline and pins to fixed cores 0/2/4 — concurrent load fails it legitimately.
    fanout=$(sed -n '/^FANOUT=(/,/^)/p' drive/gate.sh)
    [[ "$fanout" != *" h6 "* ]]
    run grep -q 'run_one "04-h6"' drive/gate.sh
    [ "$status" -eq 0 ]
}

@test "fan-out is ordered longest-first so the pool does not stall on a straggler" {
    # h5 is not in the pool at all by default — it runs alone ahead of it (see below), so
    # the pool's own longest unit, h7, must lead.
    fanout=$(sed -n '/^FANOUT=(/,/^)/p' drive/gate.sh | tr -d '\n')
    [[ "$fanout" =~ FANOUT=\(\ *h/h7 ]]
    [[ "$fanout" != *"h/h5"* ]]
}

@test "h5 runs alone before the fan-out by default" {
    # thousand_branches parallelises its own 1000 branches across worker threads, so h5
    # already saturates the box; sharing the host with other scripts inflates the very unit
    # that sets the floor (~88s alone vs ~126s at --jobs 2).
    run grep -qE '^H5_FIRST=1' drive/gate.sh
    [ "$status" -eq 0 ] || { echo "h5-first is not the default"; false; }
    run grep -q 'run_one "03-h5"' drive/gate.sh
    [ "$status" -eq 0 ]
    # ...and the old behaviour stays available for A/B.
    run grep -q -- '--no-h5-first' drive/gate.sh
    [ "$status" -eq 0 ]
}

@test "a failing unit does not abort the rest of the gate" {
    # One broken script must not hide the state of everything after it.
    run grep -q 'rc=0$' drive/gate.sh
    [ "$status" -eq 0 ]
    run grep -q '"\$@" > "\$log" 2>&1 || rc=\$?' drive/gate.sh
    [ "$status" -eq 0 ]
}

# ── pkg-build-cli gating ─────────────────────────────────────────────────────

@test "the build-cli fingerprint covers inputs no git diff can see" {
    # Its dominant input (~/wsl-kernel-src/src) is outside git, and drive/manual/h3-enforced-*.sh
    # mutates that tree in place — a path-only rule would be unsound.
    fp=$(sed -n '/^build_cli_fingerprint()/,/^}/p' drive/gate.sh)
    [[ "$fp" == *"kernelversion"* ]]
    [[ "$fp" == *"handle_baud_rdtsc_exit"* ]]
    [[ "$fp" == *"gcc-13"* ]]
    [[ "$fp" == *"minimal.config"* ]]
}

@test "the fingerprint fails safe when the kernel tree or toolchain is missing" {
    fp=$(sed -n '/^build_cli_fingerprint()/,/^}/p' drive/gate.sh)
    [[ "$fp" == *"NO-KERNEL-TREE"* ]]
    [[ "$fp" == *"NO-GCC13"* ]]
}

@test "the stamp is only written after pkg-build-cli actually passed" {
    run grep -q 'grep -q .*05-pkg-build-cli\\tPASS' drive/gate.sh
    [ "$status" -eq 0 ]
}

# ── phase 6: flake isolation re-run ──────────────────────────────────────────

@test "flake detection accepts a sole-cause failure and rejects a mixed one" {
    eval "$(sed -n '/^FLAKE_TEST="/p;/^flake_is_sole_cause()/,/^}$/p' drive/gate.sh)"
    tmp="$BATS_TEST_TMPDIR"

    printf 'failures:\n\n---- linux::tests::%s stdout ----\n' "$FLAKE_TEST" > "$tmp/sole.log"
    run flake_is_sole_cause "$tmp/sole.log"
    [ "$status" -eq 0 ]

    # The flake AND a real regression: downgrading this would hide the regression.
    printf 'failures:\n\n---- linux::tests::%s stdout ----\n---- linux::tests::other stdout ----\n' \
        "$FLAKE_TEST" > "$tmp/mixed.log"
    run flake_is_sole_cause "$tmp/mixed.log"
    [ "$status" -ne 0 ]

    # h3.sh's fail() exits on the spot, so its marker line is conclusive alone.
    printf '  [FAIL] H3.4: %s FAILED\n' "$FLAKE_TEST" > "$tmp/h3.log"
    run flake_is_sole_cause "$tmp/h3.log"
    [ "$status" -eq 0 ]

    : > "$tmp/empty.log"
    run flake_is_sole_cause "$tmp/empty.log"
    [ "$status" -ne 0 ]
}

@test "a flake still exits 1 instead of turning the gate green" {
    guard=$(grep -n 'gate not green' drive/gate.sh | cut -d: -f1)
    green=$(grep -n '^say ".*gate green' drive/gate.sh | cut -d: -f1)
    [ -n "$guard" ] && [ -n "$green" ]
    [ "$guard" -lt "$green" ]
}

# ── behavioural: interrupt cleanup (slow, needs an idle machine) ──────────────

# bats test_tags=slow
@test "slow: interrupting the gate reaps everything it started and nothing else" {
    [ -x target/debug/baud-server ] || skip "target/debug/baud-server not built"

    base_sqlite=$(ls /tmp/baud-*.sqlite* 2>/dev/null | wc -l)
    base_snap=$(ls -d /tmp/baud-*snap-* 2>/dev/null | wc -l)
    base_kvm=" $(kvm_holders) "

    # An unrelated server, exactly like one a developer left running. It must survive.
    unrelated_db="$(mktemp -u -t bats-unrelated-XXXXXX.sqlite)"
    BAUD_DB="sqlite://${unrelated_db}?mode=rwc" BAUD_ADDR="127.0.0.1:39997" \
        BAUD_LOG=warn ./target/debug/baud-server >/dev/null 2>&1 &
    unrelated_pid=$!
    sleep 3
    kill -0 "$unrelated_pid" 2>/dev/null || skip "could not start the unrelated server"

    ./drive/gate.sh --jobs 4 --skip-cargo >/tmp/bats-gate-interrupt.log 2>&1 &
    gate_pid=$!

    for _ in $(seq 1 120); do
        n=$(ps -eo args --no-headers | grep -cE 'drive/(h|m|pkg-)[a-z0-9-]*\.sh' || true)
        [ "$n" -ge 3 ] && break
        kill -0 "$gate_pid" 2>/dev/null || break
        sleep 1
    done
    sleep 4

    kill -TERM "$gate_pid" 2>/dev/null
    for _ in $(seq 1 90); do kill -0 "$gate_pid" 2>/dev/null || break; sleep 1; done
    sleep 3

    leftover=$(ps -eo args --no-headers | grep -cE 'drive/(h|m|pkg-)[a-z0-9-]*\.sh' || true)
    unrelated_alive=no; kill -0 "$unrelated_pid" 2>/dev/null && unrelated_alive=yes
    now_sqlite=$(ls /tmp/baud-*.sqlite* 2>/dev/null | wc -l)
    now_snap=$(ls -d /tmp/baud-*snap-* 2>/dev/null | wc -l)

    new_kvm=""
    for pid in $(kvm_holders); do
        case "$base_kvm" in *" $pid "*) ;; *) new_kvm="$new_kvm $pid" ;; esac
    done

    kill -TERM "$unrelated_pid" 2>/dev/null || true
    rm -f "$unrelated_db" 2>/dev/null || true

    [ "$leftover" -eq 0 ]                || { echo "orphaned drive scripts: $leftover"; false; }
    [ "$unrelated_alive" = yes ]         || { echo "the gate killed an UNRELATED baud-server"; false; }
    [ "$now_sqlite" -le "$base_sqlite" ] || { echo "leaked temp sqlite: $base_sqlite -> $now_sqlite"; false; }
    [ "$now_snap" -le "$base_snap" ]     || { echo "leaked snapshot dirs: $base_snap -> $now_snap"; false; }
    [ -z "${new_kvm// /}" ]              || { echo "gate left /dev/kvm holders:$new_kvm"; false; }
}

# bats test_tags=slow
@test "slow: a drive script cleans up after itself when signalled directly" {
    [ -x target/debug/baud-server ] || skip "target/debug/baud-server not built"

    base_sqlite=$(ls /tmp/baud-*.sqlite* 2>/dev/null | wc -l)
    base_snap=$(ls -d /tmp/baud-*snap-* 2>/dev/null | wc -l)

    BAUD_GATE_PREBUILT=1 bash drive/m/m9.sh >/dev/null 2>&1 &
    script_pid=$!
    for _ in $(seq 1 60); do
        pgrep -x baud-server >/dev/null 2>&1 && break
        kill -0 "$script_pid" 2>/dev/null || break
        sleep 0.5
    done

    kill -TERM "$script_pid" 2>/dev/null
    for _ in $(seq 1 30); do kill -0 "$script_pid" 2>/dev/null || break; sleep 0.5; done
    sleep 2

    now_sqlite=$(ls /tmp/baud-*.sqlite* 2>/dev/null | wc -l)
    now_snap=$(ls -d /tmp/baud-*snap-* 2>/dev/null | wc -l)

    [ "$now_sqlite" -le "$base_sqlite" ] || { echo "m9.sh leaked temp sqlite on TERM"; false; }
    [ "$now_snap" -le "$base_snap" ]     || { echo "m9.sh leaked its snapshot dir on TERM"; false; }
}
