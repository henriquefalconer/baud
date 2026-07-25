#!/usr/bin/env bats
#
# Unit tests for ralph/ralph.
#
# The script is one long top-level program, so the tests source only its
# definitions — everything from the config block down to the first top-level
# statement — into the test shell. That way the tests exercise the real
# functions rather than copies that can drift.
#
# The load-bearing behaviours under test:
#   * a session runs in its OWN process group, so its whole tree is addressable
#   * `wait` on the session pid returns while its subagents are still alive —
#     the actual failure that let an Opus subagent report in after the ralph
#     iteration had already moved on
#   * await_group blocks until that leftover work finishes
#   * reap_sessions kills the entire group, so stopping ralph stops every
#     `claude -p` it spawned instead of orphaning them onto init
#   * the pure helpers (fmt_dur, promise_of) behave at their boundaries

setup() {
  REPO="${BATS_TEST_DIRNAME}/.."
  RALPH="${REPO}/ralph/ralph"
  TMP="$(mktemp -d)"

  # Source the script's definitions without running the loop.
  python3 - "$RALPH" > "$TMP/lib.sh" <<'PY'
import sys
s = open(sys.argv[1]).read()
start = s.index('PROGRESS="ralph/progress.txt"')
end = s.index('# run_session <label> <prompt-file>')
block = s[start:end]
# Drop trap installation: a `trap ... EXIT` from the script replaces the one
# bats uses to record a test result, and the test silently disappears from the
# run. The traps themselves are asserted separately by grepping the script.
block = "\n".join(l for l in block.split("\n") if not l.startswith("trap "))
print(block)
PY

  PROGRESS="$TMP/progress.txt"; : > "$PROGRESS"
  COST_FILE="$TMP/costs.txt";   : > "$COST_FILE"
  RUNDIR="$TMP/run"; mkdir -p "$RUNDIR"
  MODEL=sonnet
  set -m                        # same job-control mode the script runs under
  # The extracted block includes ralph's option parser, which would otherwise
  # consume bats's own positional arguments (the test name) and abort with
  # "unknown option test_...". Clear them first.
  set --
  # shellcheck disable=SC1090
  source "$TMP/lib.sh"
  PROGRESS="$TMP/progress.txt"; COST_FILE="$TMP/costs.txt"
}

teardown() {
  [ -n "${TMP:-}" ] && rm -rf "$TMP"
  # never leave test children behind
  for p in ${SPAWNED:-}; do kill -KILL -"$p" 2>/dev/null || true; done
}

# ── the script must parse ─────────────────────────────────────────────────────

@test "ralph/ralph is valid bash" {
  run bash -n "$RALPH"
  [ "$status" -eq 0 ]
}

@test "job control is enabled (set -m) so sessions get their own process group" {
  run grep -qE '^set -m$' "$RALPH"
  [ "$status" -eq 0 ]
}

# ── fmt_dur ───────────────────────────────────────────────────────────────────

@test "fmt_dur renders minutes under an hour" {
  [ "$(fmt_dur 0)"    = "0min"  ]
  [ "$(fmt_dur 59)"   = "1min"  ]
  [ "$(fmt_dur 1800)" = "30min" ]
  [ "$(fmt_dur 2700)" = "45min" ]
}

@test "fmt_dur renders hours and carries 60 minutes into the hour" {
  [ "$(fmt_dur 3600)"  = "1h00min"  ]
  [ "$(fmt_dur 6300)"  = "1h45min"  ]
  [ "$(fmt_dur 3599)"  = "1h00min"  ]   # must not read 0h60min
  [ "$(fmt_dur 86399)" = "24h00min" ]
}

@test "fmt_dur tolerates missing or empty input" {
  [ "$(fmt_dur)"   = "0min" ]
  [ "$(fmt_dur '')" = "0min" ]
}

# ── promise_of ────────────────────────────────────────────────────────────────

@test "promise_of extracts NEXT and COMPLETE" {
  [ "$(promise_of 'blah <promise>NEXT</promise>')"     = "NEXT"     ]
  [ "$(promise_of 'done <promise>COMPLETE</promise>')" = "COMPLETE" ]
}

@test "promise_of takes the LAST tag when several appear" {
  [ "$(promise_of 'x <promise>NEXT</promise> y <promise>COMPLETE</promise>')" = "COMPLETE" ]
}

@test "promise_of returns nothing when there is no tag" {
  [ -z "$(promise_of 'Waiting for the background test run to finish.')" ]
  [ -z "$(promise_of '')" ]
  [ -z "$(promise_of 'NEXT without tags')" ]
}

# ── process groups: the core of both fixes ────────────────────────────────────

@test "a backgrounded session gets its own process group" {
  sleep 30 &
  local pid=$! pgid
  pgid="$(ps -o pgid= -p "$pid" | tr -d ' ')"
  SPAWNED="$pgid"
  [ -n "$pgid" ]
  [ "$pgid" != "$$" ]
  kill -TERM -"$pgid" 2>/dev/null || true
}

@test "a grandchild inherits the session's process group" {
  bash -c 'sleep 30 & sleep 30' &
  local pid=$! pgid
  sleep 0.5
  pgid="$(ps -o pgid= -p "$pid" | tr -d ' ')"
  SPAWNED="$pgid"
  local members
  members="$(group_members "$pgid")"
  # the job plus at least one descendant
  [ "$(printf '%s\n' $members | wc -l)" -ge 2 ]
  kill -TERM -"$pgid" 2>/dev/null || true
}

@test "group_members reports nothing for an empty or bogus group" {
  [ -z "$(group_members 999999)" ]
  [ -z "$(group_members '')" ]
  [ -z "$(group_members)" ]
}

@test "REGRESSION: wait returns while a subagent is still alive" {
  # The exact failure this work exists to fix: the session's own process exits,
  # `wait` returns, and the loop would march on — while spawned work continues.
  bash -c 'sleep 30 & sleep 1' &
  local pid=$! pgid
  sleep 0.5
  pgid="$(ps -o pgid= -p "$pid" 2>/dev/null | tr -d ' ')"
  if [ -z "$pgid" ]; then skip "job exited before pgid could be read"; fi
  SPAWNED="$pgid"
  while kill -0 "$pid" 2>/dev/null; do sleep 0.2; done   # `wait` misbehaves under set -m here
  # wait has returned, yet the group still has a live member
  [ -n "$(group_members "$pgid")" ]
  kill -TERM -"$pgid" 2>/dev/null || true
}

@test "await_group returns immediately when nothing is left" {
  local start elapsed
  start=$(date +%s)
  await_group 999999 "empty" >/dev/null 2>&1
  elapsed=$(( $(date +%s) - start ))
  [ "$elapsed" -le 2 ]
}

@test "await_group blocks until leftover work finishes" {
  bash -c 'sleep 8 & sleep 1' &
  local pid=$! pgid start elapsed
  sleep 0.5
  pgid="$(ps -o pgid= -p "$pid" 2>/dev/null | tr -d ' ')"
  if [ -z "$pgid" ]; then skip "job exited before pgid could be read"; fi
  SPAWNED="$pgid"
  while kill -0 "$pid" 2>/dev/null; do sleep 0.2; done   # `wait` misbehaves under set -m here
  start=$(date +%s)
  await_group "$pgid" "test" >/dev/null 2>&1
  elapsed=$(( $(date +%s) - start ))
  [ "$elapsed" -ge 4 ]                       # it actually waited
  [ -z "$(group_members "$pgid")" ]          # and the group is clear
}

@test "await_group gives up and kills the group at GROUP_WAIT_MAX" {
  GROUP_WAIT_MAX=5
  bash -c 'sleep 300 & sleep 1' &
  local pid=$! pgid start elapsed
  sleep 0.5
  pgid="$(ps -o pgid= -p "$pid" 2>/dev/null | tr -d ' ')"
  if [ -z "$pgid" ]; then skip "job exited before pgid could be read"; fi
  SPAWNED="$pgid"
  while kill -0 "$pid" 2>/dev/null; do sleep 0.2; done   # `wait` misbehaves under set -m here
  start=$(date +%s)
  await_group "$pgid" "stuck" >/dev/null 2>&1
  elapsed=$(( $(date +%s) - start ))
  [ "$elapsed" -lt 40 ]                      # bounded, not forever
  sleep 1
  [ -z "$(group_members "$pgid")" ]          # and it cleaned up after itself
}

# ── stopping ralph must stop its sessions ─────────────────────────────────────

@test "reap_sessions kills a whole session group, subagents included" {
  bash -c 'sleep 300 & sleep 300' &
  local pid=$! pgid
  sleep 0.5
  pgid="$(ps -o pgid= -p "$pid" | tr -d ' ')"
  SPAWNED="$pgid"
  SESSION_PGIDS=" $pgid"
  [ -n "$(group_members "$pgid")" ]          # alive before
  reap_sessions >/dev/null 2>&1
  sleep 1
  [ -z "$(group_members "$pgid")" ]          # gone after
}

@test "reap_sessions is safe with no sessions and with dead ones" {
  SESSION_PGIDS=""
  run reap_sessions
  [ "$status" -eq 0 ]
  SESSION_PGIDS=" 999999 "
  run reap_sessions
  [ "$status" -eq 0 ]
}

@test "the script traps INT, TERM and EXIT to reap sessions" {
  grep -q "trap 'reap_sessions; exit 130' INT" "$RALPH"
  grep -q "trap 'reap_sessions; exit 143' TERM" "$RALPH"
  grep -q "trap 'reap_sessions' EXIT" "$RALPH"
}

# ── ledger helpers ────────────────────────────────────────────────────────────

@test "spent_usd sums the cost ledger" {
  printf '1.50\n2.25\n' > "$COST_FILE"
  [ "$(spent_usd)" = "3.7500" ]
}

@test "spent_usd is 0 on an empty ledger" {
  : > "$COST_FILE"
  [ "$(spent_usd)" = "0.0000" ]
}

@test "budget_exhausted respects the reserve and an unset budget" {
  printf '1.50\n2.25\n' > "$COST_FILE"     # 3.75 spent
  BUDGET_RESERVE_USD=2.00
  BUDGET_TOTAL_USD=""  ; run budget_exhausted; [ "$status" -ne 0 ]   # no cap set
  BUDGET_TOTAL_USD="10"; run budget_exhausted; [ "$status" -ne 0 ]   # 6.25 left
  BUDGET_TOTAL_USD="5" ; run budget_exhausted; [ "$status" -eq 0 ]   # 1.25 left
  BUDGET_TOTAL_USD="3" ; run budget_exhausted; [ "$status" -eq 0 ]   # overspent
}

@test "jsonfield reads a field and fails cleanly on a missing one" {
  printf '{"total_cost_usd": 1.25, "session_id": "abc"}' > "$TMP/s.json"
  [ "$(jsonfield "$TMP/s.json" session_id)" = "abc" ]
  run jsonfield "$TMP/s.json" nope
  [ "$status" -ne 0 ]
  run jsonfield "$TMP/missing.json" session_id
  [ "$status" -ne 0 ]
}

# ── the usage ledger ──────────────────────────────────────────────────────────

@test "log_usage writes a header with the wall-clock duration" {
  printf '{"session_id":"s1","total_cost_usd":1.2345,"modelUsage":{}}' > "$TMP/x.json"
  SESSION_SECS=6300
  log_usage "build-9" "$TMP/x.json"
  grep -qF -- 'UTC (1h45min) - Session usage — build-9 [sonnet]' "$PROGRESS"
}

@test "log_usage prints cost to 4 decimal places" {
  printf '{"session_id":"s2","total_cost_usd":3.9363649500000006,"modelUsage":{}}' > "$TMP/y.json"
  SESSION_SECS=60
  log_usage "build-1" "$TMP/y.json"
  grep -qF -- '- cost $3.9364 (from claude -p)' "$PROGRESS"
}

@test "log_usage flags a disagreement between total_cost_usd and the per-model sum" {
  # Observed with an Opus subagent: total said 12.3886, per-model summed 16.5541.
  cat > "$TMP/z.json" <<'JSON'
{"session_id":"s3","total_cost_usd":12.3886,
 "modelUsage":{"claude-sonnet-5":{"costUSD":11.7037,"contextWindow":1000000},
               "claude-opus-5[1m]":{"costUSD":4.8474,"contextWindow":1000000}}}
JSON
  SESSION_SECS=60
  log_usage "build-2" "$TMP/z.json"
  grep -qF -- 'per-model sums to $16.5511' "$PROGRESS"
}

@test "log_usage appends the cost to the ledger file" {
  printf '{"session_id":"s4","total_cost_usd":2.5,"modelUsage":{}}' > "$TMP/w.json"
  SESSION_SECS=60
  : > "$COST_FILE"
  log_usage "build-3" "$TMP/w.json"
  [ "$(cat "$COST_FILE")" = "2.5" ]
}

@test "log_usage does nothing when the session JSON has no cost" {
  printf '{"session_id":"s5"}' > "$TMP/v.json"
  local before; before="$(wc -c < "$PROGRESS")"
  log_usage "build-4" "$TMP/v.json"
  [ "$(wc -c < "$PROGRESS")" -eq "$before" ]
}

@test "log_usage survives a truncated session JSON" {
  printf '{"session_id":' > "$TMP/u.json"
  run log_usage "build-5" "$TMP/u.json"
  [ "$status" -eq 0 ]
}

# ── run directory namespacing ─────────────────────────────────────────────────

@test "each invocation derives its own run directory" {
  grep -q 'RUN_ID="\$(date -u +%Y%m%dT%H%M%SZ)"' "$RALPH"
  grep -q 'RUNDIR="ralph/.run/\$RUN_ID"' "$RALPH"
}
