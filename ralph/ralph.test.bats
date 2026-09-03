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
#     the actual failure that let a subagent report in after the ralph
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
  # The test environment may not ship the sqlite3 CLI. Keep the SQL setup used by
  # these tests working through Python's standard-library SQLite driver.
  if ! command -v sqlite3 >/dev/null 2>&1; then
    mkdir -p "$TMP/bin"
    cat > "$TMP/bin/sqlite3" <<'PY'
#!/usr/bin/env python3
import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
sql = sys.argv[2] if len(sys.argv) > 2 else sys.stdin.read()
cur = con.executescript(sql)
if sql.lstrip().lower().startswith(("select", "pragma")):
    for row in cur.fetchall():
        print("|".join("" if x is None else str(x) for x in row))
con.commit()
PY
    chmod +x "$TMP/bin/sqlite3"
    PATH="$TMP/bin:$PATH"
  fi
  REPO_ROOT="$REPO"
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

# ── promise quiescence ───────────────────────────────────────────────────────

@test "Codex accepts a promise only after its own turn has completed and no child is open" {
  local home="$TMP/codex-home" source="$TMP/codex.jsonl"
  mkdir -p "$home"
  printf '%s\n' \
    '{"type":"thread.started","thread_id":"root"}' \
    '{"type":"turn.completed","usage":{}}' > "$source"
  sqlite3 "$home/state_5.sqlite" \
    'create table thread_spawn_edges (parent_thread_id text, child_thread_id text primary key, status text); create table threads (id text primary key, rollout_path text);'
  HARNESS=codex CODEX_HOME="$home" run codex_pending_work "$source"
  [ "$status" -eq 1 ]
}

@test "Codex refuses a promise while a spawned child has no terminal record" {
  local home="$TMP/codex-home" source="$TMP/codex.jsonl"
  mkdir -p "$home"
  printf '%s\n' \
    '{"type":"thread.started","thread_id":"root"}' \
    '{"type":"turn.completed","usage":{}}' > "$source"
  sqlite3 "$home/state_5.sqlite" \
    "create table thread_spawn_edges (parent_thread_id text, child_thread_id text primary key, status text); create table threads (id text primary key, rollout_path text); insert into thread_spawn_edges values ('root', 'child-1', 'open');"
  HARNESS=codex CODEX_HOME="$home" run codex_pending_work "$source"
  [ "$status" -eq 0 ]
  [[ "$output" == *'Codex child turn child-1 has no terminal record'* ]]
}

@test "Codex accepts a completed child even when Codex state leaves its edge open" {
  local home="$TMP/codex-home" source="$TMP/codex.jsonl" child="$TMP/child.jsonl"
  mkdir -p "$home"
  printf '%s\n' \
    '{"type":"thread.started","thread_id":"root"}' \
    '{"type":"turn.completed","usage":{}}' > "$source"
  printf '%s\n' \
    '{"timestamp":"2026-08-31T15:46:54.814Z","type":"event_msg","payload":{"type":"thread.started","thread_id":"child-1"}}' \
    '{"timestamp":"2026-08-31T15:48:21.321Z","type":"task_complete","payload":{"type":"task_complete"}}' > "$child"
  sqlite3 "$home/state_5.sqlite" \
    "create table thread_spawn_edges (parent_thread_id text, child_thread_id text primary key, status text); create table threads (id text primary key, rollout_path text); insert into thread_spawn_edges values ('root', 'child-1', 'open'); insert into threads values ('child-1', '$child');"
  HARNESS=codex CODEX_HOME="$home" run codex_pending_work "$source"
  [ "$status" -eq 1 ]
}

@test "Codex accepts a cancelled child whose stale edge never became closed" {
  local home="$TMP/codex-home" source="$TMP/codex.jsonl" child="$TMP/child.jsonl"
  mkdir -p "$home"
  printf '%s\n' \
    '{"type":"thread.started","thread_id":"root"}' \
    '{"type":"turn.completed","usage":{}}' > "$source"
  printf '%s\n' \
    '{"timestamp":"2026-08-31T16:47:56.755Z","type":"turn_context","payload":{"turn_id":"child-turn"}}' \
    '{"timestamp":"2026-08-31T16:50:22.276Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","id":"verify","status":"failed"}}}' > "$child"
  sqlite3 "$home/state_5.sqlite" \
    "create table thread_spawn_edges (parent_thread_id text, child_thread_id text primary key, status text); create table threads (id text primary key, rollout_path text); insert into thread_spawn_edges values ('root', 'child-1', 'open'); insert into threads values ('child-1', '$child');"
  HARNESS=codex CODEX_HOME="$home" run codex_pending_work "$source"
  [ "$status" -eq 1 ]
}

@test "a stale Codex edge does not print the harness-wait progress message" {
  local home="$TMP/codex-home" source="$TMP/codex.jsonl" child="$TMP/child.jsonl"
  mkdir -p "$home"
  printf '%s\n' \
    '{"type":"thread.started","thread_id":"root"}' \
    '{"type":"turn.completed","usage":{}}' > "$source"
  printf '%s\n' \
    '{"type":"event_msg","payload":{"type":"thread.started","thread_id":"child-1"}}' \
    '{"type":"task_complete","payload":{"type":"task_complete"}}' > "$child"
  sqlite3 "$home/state_5.sqlite" \
    "create table thread_spawn_edges (parent_thread_id text, child_thread_id text primary key, status text); create table threads (id text primary key, rollout_path text); insert into thread_spawn_edges values ('root', 'child-1', 'open'); insert into threads values ('child-1', '$child');"
  HARNESS=codex CODEX_HOME="$home" run await_harness_quiescence "$source" ralph-2
  [ "$status" -eq 0 ]
  ! grep -qF -- 'returned a promise while harness work remained' "$PROGRESS"
}

@test "Codex trusts the parent wait result when the state database is unavailable" {
  local source="$TMP/codex.jsonl"
  printf '%s\n' \
    '{"type":"thread.started","thread_id":"root"}' \
    '{"type":"item.completed","item":{"type":"collab_tool_call","receiver_thread_ids":["child-1"],"agents_states":{"child-1":{"status":"completed"}}}}' \
    '{"type":"turn.completed","usage":{}}' > "$source"
  HARNESS=codex CODEX_HOME="$TMP/no-state" run codex_pending_work "$source"
  [ "$status" -eq 1 ]
}

@test "Codex refuses a child that is still emitting inference events" {
  local home="$TMP/codex-home" source="$TMP/codex.jsonl" child="$TMP/child.jsonl" now
  now="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  mkdir -p "$home"
  printf '%s\n' \
    '{"type":"thread.started","thread_id":"root"}' \
    '{"type":"turn.completed","usage":{}}' > "$source"
  printf '%s\n' \
    "{\"timestamp\":\"$now\",\"type\":\"turn_context\",\"payload\":{\"turn_id\":\"child-turn\"}}" > "$child"
  sqlite3 "$home/state_5.sqlite" \
    "create table thread_spawn_edges (parent_thread_id text, child_thread_id text primary key, status text); create table threads (id text primary key, rollout_path text); insert into thread_spawn_edges values ('root', 'child-1', 'open'); insert into threads values ('child-1', '$child');"
  HARNESS=codex CODEX_HOME="$home" run codex_pending_work "$source"
  [ "$status" -eq 0 ]
  [[ "$output" == *'Codex child turn child-1 has not completed'* ]]
}

@test "Codex refuses a promise while its own turn or a live command is still running" {
  local home="$TMP/codex-home" source="$TMP/codex.jsonl"
  mkdir -p "$home"
  # The bats test shell itself remains alive while `run` invokes the helper.
  # Using its PID proves the liveness probe without leaving a test child that
  # bash waits for at teardown.
  local sleeper="$$"
  sqlite3 "$home/state_5.sqlite" \
    'create table thread_spawn_edges (parent_thread_id text, child_thread_id text primary key, status text); create table threads (id text primary key, rollout_path text);'
  printf '%s\n' \
    '{"type":"thread.started","thread_id":"root"}' \
    "{\"type\":\"item.started\",\"item\":{\"type\":\"CommandExecution\",\"id\":\"command-1\",\"process_id\":\"$sleeper\"}}" > "$source"
  HARNESS=codex CODEX_HOME="$home" run codex_pending_work "$source"
  [ "$status" -eq 0 ]
  [[ "$output" == *'Codex turn root has not completed'* ]]
  [[ "$output" == *"Codex command command-1 (pid $sleeper) is still running"* ]]
}

@test "Claude accepts a promise when its background-agent list is empty" {
  mkdir -p "$TMP/bin"
  printf '%s\n' '#!/usr/bin/env bash' 'printf "%s\\n" "$CLAUDE_AGENTS_JSON"' > "$TMP/bin/claude"
  chmod +x "$TMP/bin/claude"
  printf '%s\n' '{"session_id":"root"}' > "$TMP/claude.json"
  HARNESS=claude PATH="$TMP/bin:$PATH" CLAUDE_AGENTS_JSON='[]' run claude_pending_work "$TMP/claude.json"
  [ "$status" -eq 1 ]
}

@test "Claude refuses a promise while a descendant background agent is running" {
  mkdir -p "$TMP/bin"
  printf '%s\n' '#!/usr/bin/env bash' 'printf "%s\\n" "$CLAUDE_AGENTS_JSON"' > "$TMP/bin/claude"
  chmod +x "$TMP/bin/claude"
  printf '%s\n' '{"session_id":"root"}' > "$TMP/claude.json"
  HARNESS=claude PATH="$TMP/bin:$PATH" CLAUDE_AGENTS_JSON='[{"sessionId":"child-1","parentSessionId":"root","status":"running"}]' run claude_pending_work "$TMP/claude.json"
  [ "$status" -eq 0 ]
  [[ "$output" == *'Claude child turn child-1 is running'* ]]
}

@test "a live harness turn makes await_harness_quiescence refuse the promise" {
  local home="$TMP/codex-home" source="$TMP/codex.jsonl"
  mkdir -p "$home"
  printf '%s\n' \
    '{"type":"thread.started","thread_id":"root"}' \
    '{"type":"turn.completed","usage":{}}' > "$source"
  sqlite3 "$home/state_5.sqlite" \
    "create table thread_spawn_edges (parent_thread_id text, child_thread_id text primary key, status text); create table threads (id text primary key, rollout_path text); insert into thread_spawn_edges values ('root', 'child-1', 'open');"
  HARNESS=codex CODEX_HOME="$home" GROUP_WAIT_MAX=0 run await_harness_quiescence "$source" ralph-1
  [ "$status" -ne 0 ]
  grep -qF -- 'its promise was not accepted' "$PROGRESS"
}

@test "the loop checks harness quiescence before it accepts a promise tag" {
  wait_line=$(grep -n 'await_harness_quiescence "\$LAST_RAW_JSON" "ralph-\$ITER"' "$RALPH" | head -1 | cut -d: -f1)
  accept_line=$(grep -n '^    if \[\[ -z "\$p" \]\]; then' "$RALPH" | head -1 | cut -d: -f1)
  [ -n "$wait_line" ] && [ -n "$accept_line" ]
  [ "$wait_line" -lt "$accept_line" ]
}

@test "every main-loop prompt locks its selected entries until they are DONE" {
  for prompt in prompt-build.md prompt-plan.md; do
    grep -qF -- '**Selection lock.**' "$REPO/ralph/$prompt" || return 1
    grep -qF -- 'beginning of the main session' "$REPO/ralph/$prompt" || return 1
    grep -qF -- 'DONE' "$REPO/ralph/$prompt" || return 1
    grep -qF -- 'commit, push, and return a promise tag' "$REPO/ralph/$prompt" || return 1
  done
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
  HARNESS=claude MODEL=sonnet
  log_usage "build-9" "$TMP/x.json"
  grep -qF -- 'UTC (1h45min) - Session usage — build-9 [claude/sonnet]' "$PROGRESS"
}

@test "log_usage puts the thinking level in its own bracket after the model" {
  printf '{"session_id":"s1","total_cost_usd":1.2345,"modelUsage":{}}' > "$TMP/x.json"
  SESSION_SECS=4472
  MODEL=opus HARNESS=claude THINKING_TAG=" [low]" log_usage "build-15" "$TMP/x.json"
  grep -qF -- 'Session usage — build-15 [claude/opus] [low]' "$PROGRESS" || return 1
  # An unset level prints no second bracket at all, never an empty one.
  MODEL=opus HARNESS=claude THINKING_TAG="" log_usage "build-16" "$TMP/x.json"
  grep -qF -- 'Session usage — build-16 [claude/opus]' "$PROGRESS" || return 1
  ! grep -qF -- '[claude/opus] []' "$PROGRESS" || return 1
}

@test "log_usage prints cost to 4 decimal places" {
  printf '{"session_id":"s2","total_cost_usd":3.9363649500000006,"modelUsage":{}}' > "$TMP/y.json"
  SESSION_SECS=60
  HARNESS=claude
  log_usage "build-1" "$TMP/y.json"
  grep -qF -- '- cost $3.9364 (from harness)' "$PROGRESS"
}

@test "log_usage flags a disagreement between total_cost_usd and the per-model sum" {
  # Observed with a subagent: total said 12.3886, per-model summed 16.5541.
  cat > "$TMP/z.json" <<'JSON'
{"session_id":"s3","total_cost_usd":12.3886,
 "modelUsage":{"claude-sonnet-5":{"costUSD":11.7037,"contextWindow":1000000},
               "claude-opus-5[1m]":{"costUSD":4.8474,"contextWindow":1000000}}}
JSON
  SESSION_SECS=60
  HARNESS=claude
  log_usage "build-2" "$TMP/z.json"
  grep -qF -- 'per-model sums to $16.5511' "$PROGRESS"
}

@test "log_usage appends the cost to the ledger file" {
  printf '{"session_id":"s4","total_cost_usd":2.5,"modelUsage":{}}' > "$TMP/w.json"
  SESSION_SECS=60
  HARNESS=claude
  : > "$COST_FILE"
  log_usage "build-3" "$TMP/w.json"
  [ "$(cat "$COST_FILE")" = "2.5" ]
}

@test "log_usage does nothing when the session JSON has no cost" {
  printf '{"session_id":"s5"}' > "$TMP/v.json"
  local before; before="$(wc -c < "$PROGRESS")"
  HARNESS=claude
  log_usage "build-4" "$TMP/v.json"
  [ "$(wc -c < "$PROGRESS")" -eq "$before" ]
}

@test "log_usage survives a truncated session JSON" {
  printf '{"session_id":' > "$TMP/u.json"
  HARNESS=claude
  run log_usage "build-5" "$TMP/u.json"
  [ "$status" -eq 0 ]
}

# ── run directory namespacing ─────────────────────────────────────────────────

# ── resume-on-no-promise ─────────────────────────────────────────────────────
#
# A `claude -p` turn that ends while a Bash task is backgrounded kills that task, so a
# session reporting "waiting for the gate" has already lost it. Resuming is what saves
# the work it did before that point.

@test "resumable_session_id reads the id from a successful session JSON" {
    printf '{"is_error":false,"session_id":"abc-123","result":"waiting for the gate"}' > "$TMP/s.json"
    run resumable_session_id "$TMP/s.json"
    [ "$status" -eq 0 ]
    [ "$output" = "abc-123" ]
}

@test "resumable_session_id refuses an errored, missing or truncated session JSON" {
    printf '{"is_error":true,"session_id":"abc-123"}' > "$TMP/err.json"
    run resumable_session_id "$TMP/err.json"
    [ "$status" -ne 0 ]

    run resumable_session_id "$TMP/does-not-exist.json"
    [ "$status" -ne 0 ]

    printf '{"is_error":false,' > "$TMP/trunc.json"
    run resumable_session_id "$TMP/trunc.json"
    [ "$status" -ne 0 ]
}

@test "run_session passes --resume only when given a session id" {
    # run_session sits below the sourced region, so assert on its real text.
    fn=$(sed -n '/^run_session() {/,/^}/p' "$RALPH")
    [[ "$fn" == *'sid="${3:-}"'* ]]
    [[ "$fn" == *'resume_args=( --resume "$sid" )'* ]]
    [[ "$fn" == *'resume_args=( --session "$sid" )'* ]]
    [[ "$fn" == *'codex exec resume "$sid"'* ]]
    # An empty array must not expand to a bogus empty argument under set -u.
    [[ "$fn" == *'${resume_args[@]+"${resume_args[@]}"}'* ]]
}

@test "run_session executes Codex, captures its final message, and resumes by thread" {
    mkdir -p "$TMP/bin" "$TMP/run"
    cat > "$TMP/bin/codex" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$@" >> "$CODEX_ARGS_LOG"
out=""
for ((i=1; i<=$#; i++)); do
  if [[ "${!i}" == "-o" ]]; then j=$((i + 1)); out="${!j}"; fi
done
[[ -n "$out" ]] && printf '%s\n' 'codex says <promise>NEXT</promise>' > "$out"
printf '%s\n' '{"type":"thread.started","thread_id":"thread-test"}'
SH
    chmod +x "$TMP/bin/codex"
    printf 'prompt\n' > "$TMP/prompt.txt"
    : > "$TMP/codex-args.txt"
    export CODEX_ARGS_LOG="$TMP/codex-args.txt"
    export PATH="$TMP/bin:$PATH"
    eval "$(python3 - "$RALPH" <<'PY'
import sys
s = open(sys.argv[1]).read()
print(s[s.index('run_session() {'):s.index('# ── prompt assembly')])
PY
    )"
    HARNESS=codex MODEL=gpt-5.6-luna THINKING=low
    HARNESS_ARGS=( --yolo --json -m "$MODEL" -c "model_reasoning_effort=$THINKING" )
    RUNDIR="$TMP/run" RESULT_FILE="$TMP/result.txt" PROGRESS="$TMP/progress.txt" COST_FILE="$TMP/costs.txt"
    : > "$PROGRESS"; : > "$COST_FILE"
    run_session first "$TMP/prompt.txt"
    [ "$(cat "$RESULT_FILE")" = 'codex says <promise>NEXT</promise>' ] || return 1
    grep -q -- '--yolo' "$TMP/codex-args.txt" || return 1
    grep -q -- 'model_reasoning_effort=low' "$TMP/codex-args.txt" || return 1
    run_session second "$TMP/prompt.txt" thread-test
    awk 'BEGIN { found=0 } /resume/ { found=1 } END { exit found ? 0 : 1 }' "$TMP/codex-args.txt"
}

@test "the resume nudge is one sentence telling it not to background the command" {
    write_resume_prompt "$TMP/nudge.txt"
    [ "$(wc -l < "$TMP/nudge.txt")" -eq 1 ]
    run cat "$TMP/nudge.txt"
    [[ "$output" == *"without backgrounding"* ]]
    [[ "$output" == *"promise"* ]]
}

@test "the build loop resumes before spending a fresh session on the slot" {
    # Order matters: a restart throws away everything the stranded session did.
    resume_line=$(grep -n 'resumable_session_id "\$LAST_JSON"' "$RALPH" | head -1 | cut -d: -f1)
    fail_line=$(grep -n 'fails=\$((fails + 1))' "$RALPH" | head -1 | cut -d: -f1)
    [ -n "$resume_line" ] && [ -n "$fail_line" ]
    [ "$resume_line" -lt "$fail_line" ]
}

@test "a resumed iteration writes ONE usage entry, summing every invocation" {
    # Resume JSONs carry per-turn cost, not a running total, so they must be summed.
    printf '{"session_id":"n","total_cost_usd":0.1008,"modelUsage":{"claude-sonnet-5":{"costUSD":0.1008,"contextWindow":1000000}}}' > "$TMP/a.json"
    printf '{"session_id":"n","total_cost_usd":0.0150,"modelUsage":{"claude-sonnet-5":{"costUSD":0.0150,"contextWindow":1000000}}}' > "$TMP/b.json"
    SESSION_SECS=120
    HARNESS=claude
    log_usage "build-7" "$TMP/a.json" "$TMP/b.json"
    [ "$(grep -c 'Session usage' "$PROGRESS")" -eq 1 ]
    run grep -q 'cost \$0.1158' "$PROGRESS"
    [ "$status" -eq 0 ]
}

@test "run_session defers its usage entry while an iteration is accumulating" {
    fn=$(sed -n '/^run_session() {/,/^}/p' "$RALPH")
    [[ "$fn" == *'ITER_JSONS+=( "$out" )'* ]]
    [[ "$fn" == *'USAGE_DEFERRED'* ]]
    # begin/flush must bracket the loop body, so a resume adds no ledger entry.
    [[ "$(grep -c 'begin_iteration_usage' "$RALPH")" -ge 2 ]]
}

@test "merging invocations sums counters but not the context window" {
    printf '{"session_id":"n","total_cost_usd":0.05,"modelUsage":{"claude-sonnet-5":{"costUSD":0.05,"outputTokens":100,"contextWindow":1000000}}}' > "$TMP/a.json"
    cp "$TMP/a.json" "$TMP/b.json"
    SESSION_SECS=60
    HARNESS=claude
    log_usage "build-7" "$TMP/a.json" "$TMP/b.json"
    run grep -q 'out 200' "$PROGRESS"      # counters add
    [ "$status" -eq 0 ]
    run grep -q 'of 2M' "$PROGRESS"        # the window must not
    [ "$status" -ne 0 ]
}

@test "the ledger flags a resumed iteration even when the context looks monotonic" {
    src=$(cat "$RALPH")
    [[ "$src" == *'resumed {len(sources) - 1}x'* ]]
    # The flag must sit outside the drops branch, or a clean run hides the resume.
    [[ "$src" != *'else f"{drops} drop(s), final {ctx[-1]:,}"\n    if len(metas)'* ]]
}

@test "each invocation derives its own run directory" {
  grep -q 'RUN_ID="\$(date -u +%Y%m%dT%H%M%SZ)"' "$RALPH"
  grep -q 'RUNDIR="ralph/.run/\$RUN_ID"' "$RALPH"
}

# ── --thinking ────────────────────────────────────────────────────────────────
#
# Measured 2026-08-23 against a live Claude CLI: `--effort low` writes "effort":"low"
# on every assistant record of the transcript, while no flag inherits the user's saved
# `/effort` default — observed as "high", then "low" once that default was changed,
# so unset is not a fixed level. The level appears NOWHERE in the session JSON, so
# unless ralph names it in the ledger a past run cannot be told apart from one at
# a different level.

# Re-run the option parser in a subshell with the given argv, and report what it
# produced. cd to $TMP first: the sourced block touches progress.txt and makes a
# run directory relative to the cwd.
thinking_run() { # <argv...> -> "LABEL=<ledger label>" then translated args, one per line
    local test_harness="${RALPH_HARNESS:-pi}"
    mkdir -p "$TMP/ralph"
    : > "$TMP/ralph/prompt-build.md"   # the block refuses to load without it
    ( cd "$TMP" || exit 1
      # shellcheck disable=SC1090
      RALPH_HARNESS="$test_harness" source "$TMP/lib.sh" >/dev/null 2>&1
      printf 'LABEL=%s\n' "$MODEL_LABEL"
      printf '%s\n' "${HARNESS_ARGS[@]}" )
}

@test "default harness is pi on haiku with thinking off" {
    run thinking_run -n 1
    [ "$status" -eq 0 ] || return 1
    [[ "$output" == *"LABEL=anthropic/claude-haiku-4-5 thinking=off"* ]] || return 1
    [[ "$output" == *$'-p\n--mode\njson'* ]] || return 1
    [[ "$output" == *$'--model\nanthropic/claude-haiku-4-5'* ]] || return 1
    [[ "$output" == *$'--thinking\noff'* ]] || return 1
    grep -q 'HARNESS="\${RALPH_HARNESS:-pi}"' "$RALPH" || return 1
}

@test "explicit codex harness keeps its default model and flags" {
    RALPH_HARNESS=codex
    run thinking_run -n 1
    [ "$status" -eq 0 ] || return 1
    [[ "$output" == *"LABEL=gpt-5.6-luna"* ]] || return 1
    [[ "$output" == *"--yolo"* ]] || return 1
    [[ "$output" != *"--thinking"* ]] || return 1
}

@test "pi takes a model and a thinking level and names both in the ledger label" {
    run thinking_run --model anthropic/claude-sonnet-5 --thinking high
    [ "$status" -eq 0 ] || return 1
    [[ "$output" == *"LABEL=anthropic/claude-sonnet-5 thinking=high"* ]] || return 1
    [[ "$output" == *$'--model\nanthropic/claude-sonnet-5'* ]] || return 1
    [[ "$output" == *$'--thinking\nhigh'* ]] || return 1
    [[ "$output" != *"--effort"* ]] || return 1
    RALPH_MODEL=anthropic/claude-opus-5 run thinking_run
    [ "$status" -eq 0 ] || return 1
    [[ "$output" == *"LABEL=anthropic/claude-opus-5 thinking=off"* ]] || return 1
}

@test "pi accepts every thinking level pi has, including the two below low" {
    for level in off minimal low medium high xhigh max; do
        run thinking_run --thinking "$level"
        [ "$status" -eq 0 ]                            || return 1
        [[ "$output" == *$'--thinking\n'"$level"* ]]   || return 1
    done
    run thinking_run --thinking turbo
    [ "$status" -eq 2 ] || return 1
}

@test "off and minimal stay pi-only: claude and codex refuse them" {
    run thinking_run --harness claude --thinking off
    [ "$status" -eq 2 ] || return 1
    RALPH_HARNESS=codex run thinking_run --thinking minimal
    [ "$status" -eq 2 ] || return 1
}

@test "pi accepts shared lifecycle flags without leaking another harness's flags" {
    run thinking_run --iterations 2 --prompt prompt-build.md --watchdog 17 --budget 1.25 --budget-total 8.50
    [ "$status" -eq 0 ] || return 1
    [[ "$output" == *"LABEL=anthropic/claude-haiku-4-5 thinking=off"* ]] || return 1
    [[ "$output" != *"--max-budget-usd"* ]] || return 1
    [[ "$output" != *"--chrome"* ]] || return 1
    [[ "$output" != *"--dangerously-skip-permissions"* ]] || return 1
    [[ "$output" != *"--yolo"* ]] || return 1
    [[ "$output" != *"--effort"* ]] || return 1
    [[ "$output" != *"model_reasoning_effort"* ]] || return 1
}

@test "the session budget is checked from usage for pi and codex, never for claude" {
    printf '1.00\n' > "$COST_FILE"
    ITER_COST_START=0
    BUDGET_USD=0.50
    HARNESS=pi;     run session_budget_exhausted; [ "$status" -eq 0 ] || return 1
    HARNESS=codex;  run session_budget_exhausted; [ "$status" -eq 0 ] || return 1
    HARNESS=claude; run session_budget_exhausted; [ "$status" -ne 0 ] || return 1
    HARNESS=pi; BUDGET_USD=2.00; run session_budget_exhausted; [ "$status" -ne 0 ] || return 1
    HARNESS=pi; BUDGET_USD="";   run session_budget_exhausted; [ "$status" -ne 0 ] || return 1
}

@test "--help names pi as the default harness" {
    run "$RALPH" --help
    [ "$status" -eq 0 ] || return 1
    [[ "$output" == *"pi (default), codex or claude"* ]] || return 1
    [[ "$output" == *"-h, --help"* ]] || return 1
}

@test "explicit claude harness preserves its historical defaults" {
    run thinking_run --harness claude -n 1
    [ "$status" -eq 0 ] || return 1
    [[ "$output" == *"LABEL=sonnet"* ]] || return 1
    [[ "$output" == *$'-p\n--dangerously-skip-permissions'* ]] || return 1
    [[ "$output" == *$'--model\nsonnet'* ]] || return 1
}

@test "codex accepts an explicit model and translates the level to its config key" {
    RALPH_HARNESS=codex
    run thinking_run --model gpt-5.6-luna --thinking high
    [ "$status" -eq 0 ] || return 1
    [[ "$output" == *"LABEL=gpt-5.6-luna thinking=high"* ]] || return 1
    [[ "$output" == *$'-c\nmodel_reasoning_effort=high'* ]] || return 1
    [[ "$output" != *$'--effort\nhigh'* ]] || return 1
}

@test "Codex accepts shared lifecycle flags without leaking Claude-only flags" {
    RALPH_HARNESS=codex
    run thinking_run --iterations 2 --prompt prompt-build.md --watchdog 17
    [ "$status" -eq 0 ] || return 1
    [[ "$output" == *"LABEL=gpt-5.6-luna"* ]] || return 1
    [[ "$output" != *"--max-budget-usd"* ]] || return 1
    [[ "$output" != *"--chrome"* ]] || return 1
    [[ "$output" != *"--effort"* ]] || return 1
}

@test "Codex accepts USD budget flags and checks the measured session cost" {
    RALPH_HARNESS=codex
    run thinking_run --budget 1.25
    [ "$status" -eq 0 ] || return 1
    [[ "$output" != *"--max-budget-usd"* ]] || return 1
    run thinking_run --budget-total 8.50
    [ "$status" -eq 0 ] || return 1
}

@test "Codex usage accounting aggregates turns and child tool calls" {
    cat > "$TMP/codex-usage.jsonl" <<'JSONL'
{"type":"item.completed","item":{"type":"collab_tool_call","tool":"spawn_agent","receiver_thread_ids":["child-1"],"status":"completed"}}
{"type":"turn.completed","usage":{"input_tokens":100000,"cached_input_tokens":40000,"cache_write_input_tokens":5000,"output_tokens":20000,"reasoning_output_tokens":7000}}
{"type":"item.completed","item":{"type":"collab_tool_call","tool":"wait","receiver_thread_ids":["child-1"],"status":"completed"}}
{"type":"turn.completed","usage":{"input_tokens":80000,"cached_input_tokens":10000,"cache_write_input_tokens":2000,"output_tokens":12000,"reasoning_output_tokens":3000}}
JSONL
    HARNESS=codex MODEL=gpt-5.6-luna SESSION_SECS=60
    log_usage codex-usage "$TMP/codex-usage.jsonl"
    grep -qF -- 'cost $0.0891 (calculated from Codex token usage)' "$PROGRESS" || return 1
    grep -qF -- 'tokens input 180,000 · cache-read 50,000 · output 32,000 · reasoning 10,000' "$PROGRESS" || return 1
    grep -qF -- 'child tool calls 2; child threads 1' "$PROGRESS" || return 1
}

@test "Codex usage accounting includes persisted spawn_agent child rollouts" {
    mkdir -p "$TMP/codex-home/sessions/2026/08/31"
    cat > "$TMP/codex-home/sessions/2026/08/31/rollout-2026-08-31-child-1.jsonl" <<'JSONL'
{"payload":{"session_id":"child-1"}}
{"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100000,"cached_input_tokens":20000,"cache_write_input_tokens":0,"output_tokens":50000,"reasoning_output_tokens":10000}}}}
JSONL
    cat > "$TMP/codex-parent.jsonl" <<'JSONL'
{"type":"item.completed","item":{"type":"collab_tool_call","tool":"spawn_agent","receiver_thread_ids":["child-1"],"status":"completed"}}
{"type":"turn.completed","usage":{"input_tokens":100000,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0}}
JSONL
    export CODEX_HOME="$TMP/codex-home"
    HARNESS=codex MODEL=gpt-5.6-luna SESSION_SECS=1
    log_usage codex-parent "$TMP/codex-parent.jsonl"
    grep -qF -- 'models gpt-5.6-luna $0.1124' "$PROGRESS" || return 1
    grep -qF -- 'child threads 1' "$PROGRESS" || return 1
    [ "$(cat "$COST_FILE")" = "0.1124" ]
}

@test "the real Ralph process records a Codex run end to end" {
    mkdir -p "$TMP/fake-bin" "$TMP/ralph"
    cp "$RALPH" "$TMP/ralph/ralph"
    cp "$REPO/ralph/prompt-build.md" "$TMP/ralph/prompt-build.md"
    cat > "$TMP/fake-bin/git" <<'SH'
#!/usr/bin/env bash
if [[ "$1" == rev-parse ]]; then printf '%s\n' main; fi
exit 0
SH
    cat > "$TMP/fake-bin/codex" <<'SH'
#!/usr/bin/env bash
out=""
for ((i=1; i<=$#; i++)); do
  if [[ "${!i}" == "-o" ]]; then j=$((i + 1)); out="${!j}"; fi
done
printf '%s\n' '{"type":"thread.started","thread_id":"thread-integration"}'
printf '%s\n' '{"type":"item.completed","item":{"type":"collab_tool_call","tool":"spawn_agent","receiver_thread_ids":["child-integration"],"status":"completed"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":11,"cached_input_tokens":3,"cache_write_input_tokens":1,"output_tokens":5,"reasoning_output_tokens":2}}'
[[ -n "$out" ]] && printf '%s\n' '<promise>COMPLETE</promise>' > "$out"
SH
    chmod +x "$TMP/fake-bin/git" "$TMP/fake-bin/codex"
    mkdir -p "$TMP/empty-codex"
    run env PATH="$TMP/fake-bin:$PATH" CODEX_HOME="$TMP/empty-codex" RALPH_HARNESS=codex RALPH_MODEL=gpt-5.6-luna "$TMP/ralph/ralph" --iterations 1
    [ "$status" -eq 0 ] || return 1
    grep -qF -- 'Session usage — ralph-1 [codex/gpt-5.6-luna]' "$TMP/ralph/progress.txt" || return 1
    grep -qF -- 'tokens input 11 · cache-read 3 · output 5 · reasoning 2' "$TMP/ralph/progress.txt" || return 1
    grep -qF -- 'child tool calls 1; child threads 1' "$TMP/ralph/progress.txt" || return 1
    grep -qF -- 'total cost $0.0000 (summed from session usage)' "$TMP/ralph/progress.txt" || return 1
    grep -qF -- 'ralph ended COMPLETE' "$TMP/ralph/progress.txt" || return 1
}

@test "harness rejects unknown values before starting a session" {
    run thinking_run --harness nope
    [ "$status" -eq 2 ] || return 1
}

@test "codex output is normalized into a resumable session record" {
    cat > "$TMP/codex.jsonl" <<'JSONL'
{"type":"thread.started","thread_id":"thread-123"}
{"type":"item.completed","item":{"id":"item-1","type":"collab_tool_call","tool":"spawn_agent","sender_thread_id":"thread-123","prompt":"inspect","receiver_thread_ids":["child-1"],"status":"completed","agents_states":{"child-1":{"status":"completed","message":"done"}}}}
{"type":"turn.completed"}
JSONL
    normalize_codex_result "$TMP/codex.jsonl" "$TMP/meta.json"
    [ "$(jsonfield "$TMP/meta.json" session_id)" = "thread-123" ] || return 1
    [ "$(jsonfield "$TMP/meta.json" is_error)" = "False" ] || return 1
    [ "$(jsonfield "$TMP/meta.json" usage.input_tokens)" = "0" ] || return 1
    grep -qF -- 'collab_tool_calls' "$TMP/meta.json" || return 1
    grep -qF -- 'sender_thread_id' "$TMP/meta.json" || return 1
    grep -qF -- '"prompt": "inspect"' "$TMP/meta.json" || return 1
}

# Every assertion below carries an explicit `|| return 1`. errexit is NOT in force
# in a test body here (bats 1.14 on this box: a false `[[ ]]` in the middle of a
# test does not fail it — only the LAST command's status is read), so a bare
# assertion that is not the final line asserts nothing at all. Verified by deleting
# the `CLAUDE_ARGS+=( --effort ... )` line and watching these tests go red.
@test "no --thinking passes no level flag at all, leaving the CLI default alone" {
    run thinking_run --harness claude -n 1 --model opus
    [ "$status" -eq 0 ]                   || return 1
    [[ "$output" != *"--effort"* ]]       || return 1
    [[ "$output" == *"LABEL=opus"* ]]     || return 1
    # No level in the ledger label either, or a default run reads as a chosen one.
    [[ "$output" != *"thinking="* ]]      || return 1
}

@test "--thinking reaches claude as its --effort flag and names itself in the ledger" {
    run thinking_run --harness claude --model opus --thinking low
    [ "$status" -eq 0 ]                              || return 1
    [[ "$output" == *$'--effort\nlow'* ]]            || return 1
    [[ "$output" == *"LABEL=opus thinking=low"* ]]     || return 1
}

@test "-t is the short form of --thinking" {
    run thinking_run --harness claude -t xhigh
    [ "$status" -eq 0 ]                       || return 1
    [[ "$output" == *$'--effort\nxhigh'* ]]   || return 1
}

@test "RALPH_THINKING sets the level when no flag is given" {
    RALPH_THINKING=max run thinking_run --harness claude
    [ "$status" -eq 0 ]                     || return 1
    [[ "$output" == *$'--effort\nmax'* ]]   || return 1
}

@test "every level the CLI accepts is accepted here, and nothing else is" {
    for level in low medium high xhigh max; do
        run thinking_run --harness claude --thinking "$level"
        [ "$status" -eq 0 ]                          || return 1
        [[ "$output" == *$'--effort\n'"$level"* ]]   || return 1
    done
    # A typo must stop the run before it spends a session, not reach the CLI.
    run thinking_run --harness claude --thinking turbo
    [ "$status" -eq 2 ]                              || return 1
}

@test "the thinking level sits in harness args, not in a per-session override" {
    # One array builds every invocation, so a resume inherits the same level.
    grep -q 'HARNESS_ARGS+=( -c "model_reasoning_effort=\$THINKING" )' "$RALPH" || return 1
    grep -q 'HARNESS_ARGS+=( --effort "\$THINKING" )' "$RALPH" || return 1
    grep -q -- '--thinking "\$THINKING" )' "$RALPH" || return 1
    fn=$(sed -n '/^run_session() {/,/^}/p' "$RALPH")
    [[ "$fn" != *'--effort'* ]] || return 1
    [[ "$fn" != *'--thinking'* ]] || return 1
}

# ── pi: transcripts, children and the ledger ─────────────────────────────────
#
# pi writes one JSONL per session under <agent dir>/sessions/<encoded cwd>/, and
# every assistant message in it carries model, tokens and the USD cost pi priced that
# call at. PI_CODING_AGENT_DIR relocates the whole tree, which is how these tests
# hand the script a sessions directory of their own. The fixture reproduces, with
# the shapes measured on 2026-09-01 against pi 0.84.4, a main session that spawned
# an Agent-tool child (linked by its "parentSession" header), ran `pi -p` from bash
# (linked only by the time window of that bash call), ran `pi -p --mode json` from
# bash (its header lands in the tool result) and one Agent whose child was nested
# and never persisted (its cost survives only in the parent's tool-result details).

pi_msg() { # <entry-ts> <role> <content-json> [model] [usage-json] [extra-json]
  local ts="$1" role="$2" content="$3" model="${4:-claude-haiku-4-5}" usage="${5:-}" extra="${6:-}"
  if [[ "$role" == assistant ]]; then
    [[ -n "$usage" ]] || usage='{"input":3,"output":10,"cacheRead":20000,"cacheWrite":100,"totalTokens":20113,"cost":{"input":0.000003,"output":0.00005,"cacheRead":0.002,"cacheWrite":0.000125,"total":0.002178}}'
    printf '{"type":"message","id":"%s","timestamp":"%s","message":{"role":"assistant","content":%s,"provider":"anthropic","model":"%s","usage":%s,"stopReason":"stop"%s}}\n' \
      "$RANDOM$RANDOM" "$ts" "$content" "$model" "$usage" "$extra"
  else
    printf '{"type":"message","id":"%s","timestamp":"%s","message":{"role":"%s","content":%s%s}}\n' \
      "$RANDOM$RANDOM" "$ts" "$role" "$content" "$extra"
  fi
}

pi_fixture() { # builds $PI_HOME and $PI_MAIN, $PI_STDOUT
  PI_HOME="$TMP/pi-home"
  PI_CWD="$TMP/work"
  local sdir="$PI_HOME/sessions/--$(printf '%s' "${PI_CWD#/}" | tr '/' '-')--"
  mkdir -p "$sdir" "$PI_CWD"
  PI_MAIN="$sdir/2026-09-01T23-20-13-146Z_main-0001.jsonl"
  local agent="$sdir/2026-09-01T23-20-16-591Z_agent-0001.jsonl"
  local shell="$sdir/2026-09-01T23-20-19-984Z_shell-0001.jsonl"
  local jsonc="$sdir/2026-09-01T23-20-23-792Z_json-0001.jsonl"
  local stray="$sdir/2026-09-01T23-20-20-000Z_stray-0001.jsonl"
  local other="$sdir/2026-09-01T23-30-00-000Z_other-0001.jsonl"
  {
    printf '{"type":"session","version":3,"id":"main-0001","timestamp":"2026-09-01T23:20:13.146Z","cwd":"%s"}\n' "$PI_CWD"
    printf '{"type":"model_change","id":"m1","parentId":null,"timestamp":"2026-09-01T23:20:14.199Z","provider":"anthropic","modelId":"claude-haiku-4-5"}\n'
    printf '{"type":"thinking_level_change","id":"t1","parentId":"m1","timestamp":"2026-09-01T23:20:14.199Z","thinkingLevel":"off"}\n'
    pi_msg 2026-09-01T23:20:14.227Z user '[{"type":"text","text":"do things"}]'
    pi_msg 2026-09-01T23:20:16.549Z assistant '[{"type":"toolCall","id":"c1","name":"Agent","arguments":{"subagent_type":"general-purpose","prompt":"PONG"}}]' claude-haiku-4-5 \
      '{"input":3,"output":180,"cacheRead":0,"cacheWrite":20220,"totalTokens":20403,"cost":{"input":0.000003,"output":0.0009,"cacheRead":0,"cacheWrite":0.025275,"total":0.026178}}'
    printf '{"type":"custom","customType":"subagents:record","data":{"id":"af2e949e-becd-416","status":"completed"},"id":"x1","parentId":"c1","timestamp":"2026-09-01T23:20:17.805Z"}\n'
    pi_msg 2026-09-01T23:20:17.807Z toolResult '[{"type":"text","text":"Agent completed in 1.3s.\n\nPONG"}]' '' '' ',"toolCallId":"c1","toolName":"Agent","details":{"subagentType":"general-purpose","cost":0.0156,"status":"completed","agentId":"af2e949e-becd-416"}'
    pi_msg 2026-09-01T23:20:19.720Z assistant '[{"type":"toolCall","id":"c2","name":"bash","arguments":{"command":"pi -p --model anthropic/claude-haiku-4-5 --thinking off \"PING\""}}]' claude-haiku-4-5 \
      '{"input":6,"output":110,"cacheRead":20220,"cacheWrite":214,"totalTokens":20550,"cost":{"input":0.000006,"output":0.00055,"cacheRead":0.002022,"cacheWrite":0.0002675,"total":0.0028455}}'
    pi_msg 2026-09-01T23:20:21.855Z toolResult '[{"type":"text","text":"PING\n"}]' '' '' ',"toolCallId":"c2","toolName":"bash"'
    pi_msg 2026-09-01T23:20:23.510Z assistant '[{"type":"toolCall","id":"c3","name":"bash","arguments":{"command":"pi -p --mode json --model anthropic/claude-haiku-4-5 \"PANG\" | head -c 300"}}]' claude-haiku-4-5 \
      '{"input":6,"output":128,"cacheRead":20434,"cacheWrite":125,"totalTokens":20693,"cost":{"input":0.000006,"output":0.00064,"cacheRead":0.0020434,"cacheWrite":0.00015625,"total":0.00284565}}'
    pi_msg 2026-09-01T23:20:25.701Z toolResult '[{"type":"text","text":"{\"type\":\"session\",\"version\":3,\"id\":\"json-0001\",\"timestamp\":\"2026-09-01T23:20:23.792Z\",\"cwd\":\"/x\"}\n{\"type\":\"agent_start\"}"}]' '' '' ',"toolCallId":"c3","toolName":"bash"'
    pi_msg 2026-09-01T23:20:26.000Z assistant '[{"type":"toolCall","id":"c4","name":"Agent","arguments":{"subagent_type":"Explore","prompt":"nested"}}]' claude-haiku-4-5 \
      '{"input":1,"output":1,"cacheRead":20559,"cacheWrite":0,"totalTokens":20561,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0.0010}}'
    pi_msg 2026-09-01T23:20:26.500Z toolResult '[{"type":"text","text":"nested done"}]' '' '' ',"toolCallId":"c4","toolName":"Agent","details":{"subagentType":"Explore","cost":0.0042,"status":"completed","agentId":"deadbeef-0000-000"}'
    pi_msg 2026-09-01T23:20:27.400Z assistant '[{"type":"text","text":"All done.\n\n<promise>NEXT</promise>"}]' claude-haiku-4-5 \
      '{"input":6,"output":74,"cacheRead":20559,"cacheWrite":693,"totalTokens":21332,"cost":{"input":0.000006,"output":0.00037,"cacheRead":0.0020559,"cacheWrite":0.00086625,"total":0.00329815}}'
  } > "$PI_MAIN"
  {
    printf '{"type":"session","version":3,"id":"agent-0001","timestamp":"2026-09-01T23:20:16.591Z","cwd":"%s","parentSession":"%s"}\n' "$PI_CWD" "$PI_MAIN"
    printf '{"type":"session_info","id":"s1","parentId":null,"timestamp":"2026-09-01T23:20:16.591Z","name":"general-purpose#af2e949e"}\n'
    pi_msg 2026-09-01T23:20:16.600Z user '[{"type":"text","text":"PONG"}]'
    pi_msg 2026-09-01T23:20:17.800Z assistant '[{"type":"text","text":"PONG"}]' claude-haiku-4-5 \
      '{"input":10,"output":53,"cacheRead":0,"cacheWrite":12260,"totalTokens":12323,"cost":{"input":0.00001,"output":0.000265,"cacheRead":0,"cacheWrite":0.015325,"total":0.0156}}'
  } > "$agent"
  {
    printf '{"type":"session","version":3,"id":"shell-0001","timestamp":"2026-09-01T23:20:19.984Z","cwd":"%s"}\n' "$PI_CWD"
    pi_msg 2026-09-01T23:20:20.000Z user '[{"type":"text","text":"PING"}]'
    pi_msg 2026-09-01T23:20:21.500Z assistant '[{"type":"text","text":"PING"}]' claude-haiku-4-5 \
      '{"input":3,"output":5,"cacheRead":19733,"cacheWrite":328,"totalTokens":20069,"cost":{"input":0.000003,"output":0.000025,"cacheRead":0.0019733,"cacheWrite":0.00041,"total":0.0024113}}'
  } > "$shell"
  {
    printf '{"type":"session","version":3,"id":"json-0001","timestamp":"2026-09-01T23:20:23.792Z","cwd":"%s"}\n' "$PI_CWD"
    pi_msg 2026-09-01T23:20:24.000Z user '[{"type":"text","text":"PANG"}]'
    pi_msg 2026-09-01T23:20:25.000Z assistant '[{"type":"text","text":"PANG"}]' claude-sonnet-5 \
      '{"input":3,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":8,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0.0100}}'
  } > "$jsonc"
  # Inside the PING window but the child of some other session: never ours.
  {
    printf '{"type":"session","version":3,"id":"stray-0001","timestamp":"2026-09-01T23:20:20.000Z","cwd":"%s","parentSession":"/elsewhere/parent.jsonl"}\n' "$PI_CWD"
    pi_msg 2026-09-01T23:20:20.500Z assistant '[{"type":"text","text":"stray"}]' claude-haiku-4-5 \
      '{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":5.0}}'
  } > "$stray"
  # Outside every window: an unrelated session in the same directory.
  {
    printf '{"type":"session","version":3,"id":"other-0001","timestamp":"2026-09-01T23:30:00.000Z","cwd":"%s"}\n' "$PI_CWD"
    pi_msg 2026-09-01T23:30:01.000Z assistant '[{"type":"text","text":"other"}]' claude-haiku-4-5 \
      '{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":7.0}}'
  } > "$other"
  PI_STDOUT="$TMP/001-ralph-1.json"
  {
    printf '{"type":"session","version":3,"id":"main-0001","timestamp":"2026-09-01T23:20:13.146Z","cwd":"%s"}\n' "$PI_CWD"
    printf '{"type":"agent_start"}\n{"type":"turn_start"}\n'
    printf '{"type":"turn_end","message":{"role":"assistant","content":[{"type":"text","text":"All done.\\n\\n<promise>NEXT</promise>"}],"model":"claude-haiku-4-5","usage":{"input":6,"output":74,"cacheRead":20559,"cacheWrite":693,"totalTokens":21332,"cost":{"total":0.00329815}},"stopReason":"stop"},"toolResults":[]}\n'
    printf '{"type":"agent_end","messages":[{"role":"assistant","content":[{"type":"text","text":"All done.\\n\\n<promise>NEXT</promise>"}],"model":"claude-haiku-4-5","stopReason":"stop"}]}\n'
    printf '{"type":"agent_settled"}\n'
  } > "$PI_STDOUT"
  export PI_CODING_AGENT_DIR="$PI_HOME"
}

@test "pi finds every transcript of an iteration from the main transcript alone" {
    pi_fixture
    run pi_transcript children "$PI_MAIN"
    [ "$status" -eq 0 ] || return 1
    [[ "$output" == *$'main\tmain-0001\tclaude-haiku-4-5\t0.0362'* ]] || return 1
    [[ "$output" == *$'agent\tagent-0001\tclaude-haiku-4-5\t0.0156'* ]] || return 1
    [[ "$output" == *$'pi -p\tshell-0001\tclaude-haiku-4-5\t0.0024'* ]] || return 1
    [[ "$output" == *$'pi -p\tjson-0001\tclaude-sonnet-5\t0.0100'* ]] || return 1
    [[ "$output" == *$'fallback\tExplore#deadbeef\t\t0.0042'* ]] || return 1
    [[ "$output" != *"stray-0001"* ]] || return 1
    [[ "$output" != *"other-0001"* ]] || return 1
}

@test "pi ledger sums main, Agent children, shell-launched children and unpersisted agents" {
    pi_fixture
    HARNESS=pi MODEL=anthropic/claude-haiku-4-5 THINKING_TAG=" [off]" SESSION_SECS=75
    : > "$COST_FILE"
    log_usage ralph-1 "$PI_STDOUT"
    grep -qF -- 'UTC (1min) - Session usage — ralph-1 [pi/anthropic/claude-haiku-4-5] [off]' "$PROGRESS" || return 1
    # 0.0361673 + 0.0156 + 0.0024113 + 0.0100 + 0.0042 = 0.0684
    grep -qF -- '- cost $0.0684 (summed from 4 session transcripts: main $0.0362 · subagents $0.0280 · 1 agent without a transcript $0.0042 from the parent'"'"'s own record)' "$PROGRESS" || return 1
    [ "$(cat "$COST_FILE")" = "0.0684" ] || return 1
    grep -qF -- '- tokens input 38 · cache-read 101,505 · cache-write 33,840 · output 556 · reasoning 0' "$PROGRESS" || return 1
    grep -qE -- '- context 21,258 tok peak( of 200K \(11%\))? across 5 inference calls, monotonic$' "$PROGRESS" || return 1
    grep -qF -- '- models claude-haiku-4-5 $0.0542 (in 35 / out 551 / cache-read 101,505) · claude-sonnet-5 $0.0100' "$PROGRESS" || return 1
    grep -qF -- '- subagents (4) pi -p x2 · agent x1 · agent (no transcript) x1' "$PROGRESS" || return 1
    grep -qF -- '  - general-purpose#af2e949e: claude-haiku-4-5 $0.0156, 1 inference calls' "$PROGRESS" || return 1
    grep -qF -- '  - Explore#deadbeef: $0.0042 (nested, not persisted)' "$PROGRESS" || return 1
    grep -qF -- "- transcript: $PI_STDOUT" "$PROGRESS" || return 1
    # The ledger names each transcript's cost, never its path: the path is the
    # machinery, and a line per subagent already says which child it was.
    ! grep -qF -- ' — /' "$PROGRESS" || return 1
    ! grep -qF -- '- session: ' "$PROGRESS" || return 1
    ! grep -qF -- 'ended with an error' "$PROGRESS" || return 1
}

@test "pi ledger flags a resumed iteration and reads the session from the last stdout" {
    pi_fixture
    printf '{"type":"agent_end","messages":[]}\n' > "$TMP/000-first.json"   # a cut-off first turn
    HARNESS=pi MODEL=anthropic/claude-haiku-4-5 THINKING_TAG="" SESSION_SECS=10
    log_usage ralph-2 "$TMP/000-first.json" "$PI_STDOUT"
    [ "$(grep -c 'Session usage' "$PROGRESS")" -eq 1 ] || return 1
    grep -qE -- 'across 5 inference calls, monotonic, resumed 1x$' "$PROGRESS" || return 1
    grep -qF -- '- cost $0.0684' "$PROGRESS" || return 1
}

@test "pi ledger reports an errored last call and tolerates missing transcripts" {
    pi_fixture
    printf '%s\n' '{"type":"session","version":3,"id":"main-0001","timestamp":"2026-09-01T23:20:13.146Z","cwd":"/x"}' > "$TMP/s.json"
    pi_msg 2026-09-01T23:20:30.000Z assistant '[]' claude-haiku-4-5 \
      '{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}' \
      ',"errorMessage":"rate limited"' | sed 's/"stopReason":"stop"/"stopReason":"error"/' >> "$PI_MAIN"
    HARNESS=pi MODEL=anthropic/claude-haiku-4-5 THINKING_TAG="" SESSION_SECS=10
    log_usage ralph-3 "$TMP/s.json"
    grep -qF -- '- ended with an error: rate limited' "$PROGRESS" || return 1
    # No session header at all: an entry still lands, saying why it is empty.
    printf '{"type":"agent_end","messages":[]}\n' > "$TMP/none.json"
    log_usage ralph-4 "$TMP/none.json"
    grep -qF -- '- session record unavailable' "$PROGRESS" || return 1
    # A header whose transcript is gone.
    printf '{"type":"session","version":3,"id":"gone-0001","timestamp":"2026-09-01T23:20:13.146Z","cwd":"/x"}\n' > "$TMP/gone.json"
    log_usage ralph-5 "$TMP/gone.json"
    grep -qF -- '- session gone-0001 has no transcript under' "$PROGRESS" || return 1
}

@test "a shell-launched pi child is matched by --session-id and --session-dir when the command names them" {
    pi_fixture
    local alt="$TMP/alt-sessions"; mkdir -p "$alt"
    {
      printf '{"type":"session","version":3,"id":"named-0001","timestamp":"2026-09-01T22:00:00.000Z","cwd":"%s"}\n' "$PI_CWD"
      pi_msg 2026-09-01T22:00:01.000Z assistant '[{"type":"text","text":"named"}]' claude-haiku-4-5 \
        '{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0.5}}'
    } > "$alt/whatever_named-0001.jsonl"
    {
      pi_msg 2026-09-01T23:20:28.000Z assistant "[{\"type\":\"toolCall\",\"id\":\"c9\",\"name\":\"bash\",\"arguments\":{\"command\":\"pi -p --session-id named-0001 --session-dir $alt hi\"}}]"
      pi_msg 2026-09-01T23:20:29.000Z toolResult '[{"type":"text","text":"ok"}]' '' '' ',"toolCallId":"c9","toolName":"bash"'
    } >> "$PI_MAIN"
    run pi_transcript children "$PI_MAIN"
    [ "$status" -eq 0 ] || return 1
    [[ "$output" == *$'pi -p\tnamed-0001\tclaude-haiku-4-5\t0.5000'* ]] || return 1
}

@test "a shell command that is not pi, or runs pi with --no-session, attributes nothing" {
    pi_fixture
    {
      pi_msg 2026-09-01T23:29:59.000Z assistant '[{"type":"toolCall","id":"c7","name":"bash","arguments":{"command":"pip install pi-tools && echo done"}}]'
      pi_msg 2026-09-01T23:30:05.000Z toolResult '[{"type":"text","text":"ok"}]' '' '' ',"toolCallId":"c7","toolName":"bash"'
    } >> "$PI_MAIN"
    run pi_transcript children "$PI_MAIN"
    [[ "$output" != *"other-0001"* ]] || return 1
    {
      pi_msg 2026-09-01T23:29:59.000Z assistant '[{"type":"toolCall","id":"c8","name":"bash","arguments":{"command":"pi -p --no-session hi"}}]'
      pi_msg 2026-09-01T23:30:05.000Z toolResult '[{"type":"text","text":"ok"}]' '' '' ',"toolCallId":"c8","toolName":"bash"'
    } >> "$PI_MAIN"
    run pi_transcript children "$PI_MAIN"
    [[ "$output" != *"other-0001"* ]] || return 1
    # And the same window WITHOUT --no-session does claim it: the window is the link.
    {
      pi_msg 2026-09-01T23:29:59.000Z assistant '[{"type":"toolCall","id":"c9","name":"bash","arguments":{"command":"cd sub && pi -p hi"}}]'
      pi_msg 2026-09-01T23:30:05.000Z toolResult '[{"type":"text","text":"ok"}]' '' '' ',"toolCallId":"c9","toolName":"bash"'
    } >> "$PI_MAIN"
    run pi_transcript children "$PI_MAIN"
    [[ "$output" == *"other-0001"* ]] || return 1
}

@test "pi children are found recursively through a child's own transcript" {
    pi_fixture
    local sdir; sdir="$(dirname "$PI_MAIN")"
    local grandchild="$sdir/2026-09-01T23-20-17-000Z_grand-0001.jsonl"
    {
      printf '{"type":"session","version":3,"id":"grand-0001","timestamp":"2026-09-01T23:20:17.000Z","cwd":"%s","parentSession":"%s"}\n' "$PI_CWD" "$sdir/2026-09-01T23-20-16-591Z_agent-0001.jsonl"
      pi_msg 2026-09-01T23:20:17.500Z assistant '[{"type":"text","text":"grand"}]' claude-haiku-4-5 \
        '{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0.25}}'
    } > "$grandchild"
    run pi_transcript children "$PI_MAIN"
    [[ "$output" == *$'agent\tgrand-0001\tclaude-haiku-4-5\t0.2500'* ]] || return 1
    HARNESS=pi MODEL=m THINKING_TAG="" SESSION_SECS=1
    log_usage ralph-6 "$PI_STDOUT"
    grep -qF -- '- cost $0.3184 (summed from 5 session transcripts' "$PROGRESS" || return 1
}

# ── pi: normalized result ─────────────────────────────────────────────────────

@test "pi output is normalized into a resumable session record" {
    pi_fixture
    normalize_pi_result "$PI_STDOUT" "$TMP/meta.json"
    [ "$(jsonfield "$TMP/meta.json" session_id)" = "main-0001" ] || return 1
    [ "$(jsonfield "$TMP/meta.json" is_error)" = "False" ] || return 1
    [ "$(jsonfield "$TMP/meta.json" session_file)" = "$PI_MAIN" ] || return 1
    [ "$(jsonfield "$TMP/meta.json" cwd)" = "$PI_CWD" ] || return 1
    [ "$(promise_of "$(jsonfield "$TMP/meta.json" result)")" = "NEXT" ] || return 1
    run resumable_session_id "$TMP/meta.json"
    [ "$status" -eq 0 ] && [ "$output" = "main-0001" ] || return 1
}

@test "pi marks an errored model call as not resumable even though the process exited 0" {
    # Measured: an unknown model id returns rc=0 with stopReason "error" on the message.
    printf '%s\n' \
      '{"type":"session","version":3,"id":"err-0001","timestamp":"2026-09-01T23:21:48.721Z","cwd":"/x"}' \
      '{"type":"message_end","message":{"role":"assistant","content":[],"model":"claude-nope-9","usage":{"cost":{"total":0}},"stopReason":"error","errorMessage":"no such model"}}' \
      '{"type":"agent_end","messages":[{"role":"assistant","content":[],"stopReason":"error","errorMessage":"no such model"}]}' > "$TMP/err.json"
    normalize_pi_result "$TMP/err.json" "$TMP/err.meta"
    [ "$(jsonfield "$TMP/err.meta" is_error)" = "True" ] || return 1
    [ "$(jsonfield "$TMP/err.meta" error)" = "no such model" ] || return 1
    run resumable_session_id "$TMP/err.meta"
    [ "$status" -ne 0 ] || return 1
}

@test "pi falls back to the transcript on disk when stdout was cut off mid-turn" {
    pi_fixture
    printf '{"type":"session","version":3,"id":"main-0001","timestamp":"2026-09-01T23:20:13.146Z","cwd":"%s"}\n{"type":"agent_start"}\n' "$PI_CWD" > "$TMP/cut.json"
    normalize_pi_result "$TMP/cut.json" "$TMP/cut.meta"
    [ "$(jsonfield "$TMP/cut.meta" is_error)" = "False" ] || return 1
    [ "$(promise_of "$(jsonfield "$TMP/cut.meta" result)")" = "NEXT" ] || return 1
    # No header at all is not a session: the record is refused outright.
    printf '{"type":"agent_start"}\n' > "$TMP/nohdr.json"
    run normalize_pi_result "$TMP/nohdr.json" "$TMP/nohdr.meta"
    [ "$status" -ne 0 ] || return 1
    # A session that never produced an assistant message is an error, not a promise.
    printf '{"type":"session","version":3,"id":"empty-0001","timestamp":"2026-09-01T23:20:13.146Z","cwd":"/x"}\n' > "$TMP/empty.json"
    normalize_pi_result "$TMP/empty.json" "$TMP/empty.meta"
    [ "$(jsonfield "$TMP/empty.meta" is_error)" = "True" ] || return 1
}

# ── pi: promise quiescence ────────────────────────────────────────────────────

@test "pi accepts a promise when every child finished and no task is running" {
    pi_fixture
    normalize_pi_result "$PI_STDOUT" "$TMP/meta.json"
    HARNESS=pi run pi_pending_work "$TMP/meta.json"
    [ "$status" -eq 1 ] || return 1
    HARNESS=pi run harness_pending_work "$TMP/meta.json"
    [ "$status" -eq 1 ] || return 1
}

@test "pi refuses, without waiting, a promise whose background Agent died with the process" {
    # Measured: `pi -p` exits as soon as the turn ends, taking an in-process
    # background agent with it, and its record never reaches a terminal status.
    pi_fixture
    {
      pi_msg 2026-09-01T23:20:28.000Z assistant '[{"type":"toolCall","id":"c5","name":"Agent","arguments":{"run_in_background":true,"prompt":"slow"}}]'
      pi_msg 2026-09-01T23:20:28.100Z toolResult '[{"type":"text","text":"Agent started in background.\nAgent ID: 4cd2b718-6d5e-47c"}]' '' '' ',"toolCallId":"c5","toolName":"Agent","details":{"status":"background","agentId":"4cd2b718-6d5e-47c","description":"slow job"}'
      pi_msg 2026-09-01T23:20:29.000Z assistant '[{"type":"text","text":"<promise>NEXT</promise>"}]'
    } >> "$PI_MAIN"
    normalize_pi_result "$PI_STDOUT" "$TMP/meta.json"
    HARNESS=pi run pi_pending_work "$TMP/meta.json"
    [ "$status" -eq 0 ] || return 1
    [[ "$output" == 'lost: pi background agent 4cd2b718-6d5e-47c (slow job) was still running when the session exited' ]] || return 1
    local start elapsed
    start=$(date +%s)
    HARNESS=pi GROUP_WAIT_MAX=900 run await_harness_quiescence "$TMP/meta.json" ralph-1
    elapsed=$(( $(date +%s) - start ))
    [ "$status" -ne 0 ] || return 1
    [ "$elapsed" -le 3 ] || return 1
    grep -qF -- 'ended with backgrounded work unfinished' "$PROGRESS" || return 1
    grep -qF -- 'its promise was not accepted' "$PROGRESS" || return 1
    # The same agent, once its record says it finished, no longer blocks.
    printf '{"type":"custom","customType":"subagents:record","data":{"id":"4cd2b718-6d5e-47c","status":"completed"},"id":"x2","parentId":"c5","timestamp":"2026-09-01T23:20:40.000Z"}\n' >> "$PI_MAIN"
    HARNESS=pi run pi_pending_work "$TMP/meta.json"
    [ "$status" -eq 1 ] || return 1
}

@test "a background Agent from an earlier turn does not block a resumed pi session" {
    # Measured 2026-09-01: the record of an agent cut off in turn 1 can never change,
    # so a resumed turn that redid the work in the foreground was refused three times
    # in a row and the slot aborted. Only the last turn's background agents count.
    pi_fixture
    {
      pi_msg 2026-09-01T23:20:28.000Z assistant '[{"type":"toolCall","id":"c5","name":"Agent","arguments":{"run_in_background":true,"prompt":"slow"}}]'
      pi_msg 2026-09-01T23:20:28.100Z toolResult '[{"type":"text","text":"Agent started in background."}]' '' '' ',"toolCallId":"c5","toolName":"Agent","details":{"status":"background","agentId":"4cd2b718-6d5e-47c","description":"slow job"}'
      pi_msg 2026-09-01T23:20:29.000Z assistant '[{"type":"text","text":"<promise>NEXT</promise>"}]'
    } >> "$PI_MAIN"
    normalize_pi_result "$PI_STDOUT" "$TMP/meta.json"
    HARNESS=pi run pi_pending_work "$TMP/meta.json"
    [ "$status" -eq 0 ] || return 1
    # The resume nudge is a new user turn; the foreground redo follows it.
    {
      pi_msg 2026-09-01T23:21:00.000Z user '[{"type":"text","text":"Repeat the command without backgrounding it"}]'
      pi_msg 2026-09-01T23:21:01.000Z assistant '[{"type":"toolCall","id":"c6","name":"bash","arguments":{"command":"sleep 2"}}]'
      pi_msg 2026-09-01T23:21:03.000Z toolResult '[{"type":"text","text":""}]' '' '' ',"toolCallId":"c6","toolName":"bash"'
      pi_msg 2026-09-01T23:21:04.000Z assistant '[{"type":"text","text":"<promise>NEXT</promise>"}]'
    } >> "$PI_MAIN"
    HARNESS=pi run pi_pending_work "$TMP/meta.json"
    [ "$status" -eq 1 ] || return 1
    # But backgrounding it AGAIN in the resumed turn is refused again.
    {
      pi_msg 2026-09-01T23:21:05.000Z assistant '[{"type":"toolCall","id":"c7","name":"Agent","arguments":{"run_in_background":true,"prompt":"slow"}}]'
      pi_msg 2026-09-01T23:21:05.100Z toolResult '[{"type":"text","text":"Agent started in background."}]' '' '' ',"toolCallId":"c7","toolName":"Agent","details":{"status":"background","agentId":"99999999-0000-000","description":"slow again"}'
    } >> "$PI_MAIN"
    HARNESS=pi run pi_pending_work "$TMP/meta.json"
    [ "$status" -eq 0 ] || return 1
    [[ "$output" == *'99999999-0000-000'* ]] || return 1
    [[ "$output" != *'4cd2b718'* ]] || return 1
}

@test "pi waits on a detached background task only while its pid is alive" {
    pi_fixture
    local tdir="$PI_CWD/.pi/tasks/main-0001-4242"; mkdir -p "$tdir"
    normalize_pi_result "$PI_STDOUT" "$TMP/meta.json"
    # The bats shell itself is the live process; nothing to clean up afterwards.
    printf '{"id":"task-1","status":"running","pid":%s,"command":"sleep 300"}\n' "$$" > "$tdir/task-1.json"
    HARNESS=pi run pi_pending_work "$TMP/meta.json"
    [ "$status" -eq 0 ] || return 1
    [[ "$output" == "pi background task task-1 (pid $$) is still running" ]] || return 1
    [[ "$output" != lost:* ]] || return 1
    printf '{"id":"task-1","status":"running","pid":999999,"command":"sleep 300"}\n' > "$tdir/task-1.json"
    HARNESS=pi run pi_pending_work "$TMP/meta.json"
    [ "$status" -eq 1 ] || return 1
    printf '{"id":"task-1","status":"completed","pid":%s,"command":"sleep 300"}\n' "$$" > "$tdir/task-1.json"
    HARNESS=pi run pi_pending_work "$TMP/meta.json"
    [ "$status" -eq 1 ] || return 1
}

@test "pi reports an unreadable session record as pending rather than accepting blindly" {
    HARNESS=pi run pi_pending_work "$TMP/does-not-exist.meta"
    [ "$status" -eq 0 ] || return 1
    [[ "$output" == *'unavailable'* ]] || return 1
    printf '{"session_id":"nowhere-0001"}' > "$TMP/nowhere.meta"
    PI_CODING_AGENT_DIR="$TMP/empty-home" HARNESS=pi run pi_pending_work "$TMP/nowhere.meta"
    [ "$status" -eq 0 ] || return 1
    [[ "$output" == *'has no transcript'* ]] || return 1
}

# ── pi: the session runner and the whole process ─────────────────────────────

# A stand-in pi that behaves like the real one where ralph can tell: it logs its
# argv, reads the prompt from stdin, streams the JSON events to stdout, and writes
# the session transcript under PI_CODING_AGENT_DIR the way pi names it.
write_fake_pi() { # <bin dir>
  cat > "$1/pi" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$@" >> "$PI_ARGS_LOG"
printf 'env RALPH_MODEL=%s\nenv RALPH_THINKING=%s\n' "${RALPH_MODEL:-}" "${RALPH_THINKING:-}" >> "$PI_ARGS_LOG"
if [[ "$1" == --offline && "$2" == --list-models ]]; then
  printf 'provider   model  context  max-out  thinking  images\nanthropic  claude-haiku-4-5  200K  64K  yes  yes\n'; exit 0
fi
sid="fake-$(date +%s%N 2>/dev/null || date +%s)-$RANDOM"
for ((i=1; i<=$#; i++)); do
  if [[ "${!i}" == "--session" ]]; then j=$((i + 1)); sid="${!j}"; fi
done
prompt="$(cat)"
cwd="$PWD"
dir="$PI_CODING_AGENT_DIR/sessions/--$(printf '%s' "${cwd#/}" | tr '/' '-')--"
mkdir -p "$dir"
file="$(ls "$dir"/*_"$sid".jsonl 2>/dev/null | head -1)"
now="$(date -u +%Y-%m-%dT%H:%M:%S.000Z)"
if [[ -z "$file" ]]; then
  file="$dir/$(date -u +%Y-%m-%dT%H-%M-%S)-000Z_$sid.jsonl"
  printf '{"type":"session","version":3,"id":"%s","timestamp":"%s","cwd":"%s"}\n' "$sid" "$now" "$cwd" > "$file"
fi
reply="${PI_FAKE_REPLY:-<promise>COMPLETE</promise>}"
[[ "$prompt" == *"without backgrounding"* ]] && reply="${PI_FAKE_RESUME_REPLY:-$reply}"
usage='{"input":11,"output":5,"cacheRead":3,"cacheWrite":1,"totalTokens":20,"cost":{"input":0.000011,"output":0.000025,"cacheRead":0.0000003,"cacheWrite":0.00000125,"total":0.02}}'
printf '{"type":"message","id":"%s","timestamp":"%s","message":{"role":"user","content":[{"type":"text","text":"prompt"}]}}\n' "$RANDOM" "$now" >> "$file"
printf '{"type":"message","id":"%s","timestamp":"%s","message":{"role":"assistant","content":[{"type":"text","text":"%s"}],"provider":"anthropic","model":"claude-haiku-4-5","usage":%s,"stopReason":"stop"}}\n' "$RANDOM" "$now" "$reply" "$usage" >> "$file"
printf '{"type":"session","version":3,"id":"%s","timestamp":"%s","cwd":"%s"}\n' "$sid" "$now" "$cwd"
printf '{"type":"agent_start"}\n{"type":"turn_start"}\n'
printf '{"type":"turn_end","message":{"role":"assistant","content":[{"type":"text","text":"%s"}],"model":"claude-haiku-4-5","usage":%s,"stopReason":"stop"},"toolResults":[]}\n' "$reply" "$usage"
printf '{"type":"agent_end","messages":[{"role":"assistant","content":[{"type":"text","text":"%s"}],"model":"claude-haiku-4-5","stopReason":"stop"}]}\n' "$reply"
printf '{"type":"agent_settled"}\n'
SH
  chmod +x "$1/pi"
}

@test "run_session executes pi, captures its final message, and resumes by --session" {
    mkdir -p "$TMP/bin" "$TMP/run" "$TMP/pi-home"
    write_fake_pi "$TMP/bin"
    printf 'prompt\n' > "$TMP/prompt.txt"
    : > "$TMP/pi-args.txt"
    export PI_ARGS_LOG="$TMP/pi-args.txt" PI_CODING_AGENT_DIR="$TMP/pi-home" PI_FAKE_REPLY='pi says <promise>NEXT</promise>'
    export PATH="$TMP/bin:$PATH"
    eval "$(python3 - "$RALPH" <<'PY'
import sys
s = open(sys.argv[1]).read()
print(s[s.index('run_session() {'):s.index('# ── prompt assembly')])
PY
    )"
    HARNESS=pi MODEL=anthropic/claude-haiku-4-5 THINKING=off MODEL_LABEL="anthropic/claude-haiku-4-5 thinking=off"
    HARNESS_ARGS=( -p --mode json --model "$MODEL" --thinking "$THINKING" )
    RUNDIR="$TMP/run" RESULT_FILE="$TMP/result.txt" PROGRESS="$TMP/progress.txt" COST_FILE="$TMP/costs.txt"
    : > "$PROGRESS"; : > "$COST_FILE"
    run_session first "$TMP/prompt.txt"
    [ "$(cat "$RESULT_FILE")" = 'pi says <promise>NEXT</promise>' ] || return 1
    grep -qx -- '--thinking' "$TMP/pi-args.txt" || return 1
    grep -qx -- 'json' "$TMP/pi-args.txt" || return 1
    ! grep -q -- '--resume' "$TMP/pi-args.txt" || return 1
    local sid; sid="$(jsonfield "$RUNDIR/001-first.json.meta" session_id)"
    [ -n "$sid" ] || return 1
    [ "$LAST_JSON" = "$RUNDIR/001-first.json.meta" ] || return 1
    [ -f "$(jsonfield "$RUNDIR/001-first.json.meta" session_file)" ] || return 1
    # Not deferred: one ledger entry per invocation, priced from the transcript.
    grep -qF -- 'Session usage — first [pi/anthropic/claude-haiku-4-5]' "$PROGRESS" || return 1
    grep -qF -- '- cost $0.0200 (summed from 1 session transcript: main $0.0200)' "$PROGRESS" || return 1
    [ "$(cat "$COST_FILE")" = "0.0200" ] || return 1
    run_session second "$TMP/prompt.txt" "$sid"
    grep -qx -- '--session' "$TMP/pi-args.txt" || return 1
    grep -qx -- "$sid" "$TMP/pi-args.txt" || return 1
    ! grep -q -- '--resume' "$TMP/pi-args.txt" || return 1
    # The resumed turn reopened the same transcript, so it now holds two calls.
    [ "$(grep -c '"role":"assistant"' "$(jsonfield "$RUNDIR/002-second.json.meta" session_file)")" -eq 2 ] || return 1
}

@test "the real Ralph process records a pi run end to end" {
    mkdir -p "$TMP/fake-bin" "$TMP/ralph" "$TMP/pi-home"
    cp "$RALPH" "$TMP/ralph/ralph"
    cp "$REPO/ralph/prompt-build.md" "$TMP/ralph/prompt-build.md"
    cat > "$TMP/fake-bin/git" <<'SH'
#!/usr/bin/env bash
if [[ "$1" == rev-parse ]]; then printf '%s\n' main; fi
exit 0
SH
    chmod +x "$TMP/fake-bin/git"
    write_fake_pi "$TMP/fake-bin"
    : > "$TMP/pi-args.txt"
    run env PATH="$TMP/fake-bin:$PATH" PI_ARGS_LOG="$TMP/pi-args.txt" PI_CODING_AGENT_DIR="$TMP/pi-home" "$TMP/ralph/ralph" --iterations 1
    [ "$status" -eq 0 ] || return 1
    grep -qF -- 'ralph (shell loop) starting — harness=pi prompt=prompt-build.md model=anthropic/claude-haiku-4-5 thinking=off' "$TMP/ralph/progress.txt" || return 1
    grep -qF -- 'Session usage — ralph-1 [pi/anthropic/claude-haiku-4-5] [off]' "$TMP/ralph/progress.txt" || return 1
    grep -qF -- '- cost $0.0200 (summed from 1 session transcript: main $0.0200)' "$TMP/ralph/progress.txt" || return 1
    grep -qF -- '- tokens input 11 · cache-read 3 · cache-write 1 · output 5 · reasoning 0' "$TMP/ralph/progress.txt" || return 1
    grep -qF -- '- context 15 tok peak of 200K (0%) across 1 inference calls, monotonic' "$TMP/ralph/progress.txt" || return 1
    ! grep -qF -- '- session: ' "$TMP/ralph/progress.txt" || return 1
    # The session's environment names the model and level for the children it starts.
    grep -qx -- 'env RALPH_MODEL=anthropic/claude-haiku-4-5' "$TMP/pi-args.txt" || return 1
    grep -qx -- 'env RALPH_THINKING=off' "$TMP/pi-args.txt" || return 1
    grep -qF -- 'total cost $0.0200 (summed from session usage)' "$TMP/ralph/progress.txt" || return 1
    grep -qF -- 'ralph ended COMPLETE' "$TMP/ralph/progress.txt" || return 1
    # The flags pi received are the defaults, nothing else.
    grep -qx -- '--thinking' "$TMP/pi-args.txt" || return 1
    grep -qx -- 'off' "$TMP/pi-args.txt" || return 1
    grep -qx -- 'anthropic/claude-haiku-4-5' "$TMP/pi-args.txt" || return 1
    ! grep -q -- '--effort\|--chrome\|--yolo\|--max-budget-usd' "$TMP/pi-args.txt" || return 1
}

@test "the real Ralph process resumes a pi session that ended without a promise" {
    mkdir -p "$TMP/fake-bin" "$TMP/ralph" "$TMP/pi-home"
    cp "$RALPH" "$TMP/ralph/ralph"
    cp "$REPO/ralph/prompt-build.md" "$TMP/ralph/prompt-build.md"
    printf '#!/usr/bin/env bash\nif [[ "$1" == rev-parse ]]; then printf "%%s\\n" main; fi\nexit 0\n' > "$TMP/fake-bin/git"
    chmod +x "$TMP/fake-bin/git"
    write_fake_pi "$TMP/fake-bin"
    : > "$TMP/pi-args.txt"
    run env PATH="$TMP/fake-bin:$PATH" PI_ARGS_LOG="$TMP/pi-args.txt" PI_CODING_AGENT_DIR="$TMP/pi-home" \
        PI_FAKE_REPLY='waiting on the gate' PI_FAKE_RESUME_REPLY='<promise>COMPLETE</promise>' \
        "$TMP/ralph/ralph" --iterations 1 --thinking minimal --model anthropic/claude-sonnet-5
    [ "$status" -eq 0 ] || return 1
    grep -qx -- '--session' "$TMP/pi-args.txt" || return 1
    grep -qx -- 'minimal' "$TMP/pi-args.txt" || return 1
    # One iteration, two invocations, ONE ledger entry summing the whole transcript.
    [ "$(grep -c 'Session usage' "$TMP/ralph/progress.txt")" -eq 1 ] || return 1
    grep -qF -- 'Session usage — ralph-1 [pi/anthropic/claude-sonnet-5] [minimal]' "$TMP/ralph/progress.txt" || return 1
    grep -qF -- '- cost $0.0400 (summed from 1 session transcript: main $0.0400)' "$TMP/ralph/progress.txt" || return 1
    grep -qF -- 'across 2 inference calls, monotonic, resumed 1x' "$TMP/ralph/progress.txt" || return 1
    grep -qF -- 'sessions: 2; ralph iterations: 1' "$TMP/ralph/progress.txt" || return 1
    grep -qF -- 'ralph ended COMPLETE' "$TMP/ralph/progress.txt" || return 1
}

@test "the real Ralph process stops a pi run at the per-session budget" {
    mkdir -p "$TMP/fake-bin" "$TMP/ralph" "$TMP/pi-home"
    cp "$RALPH" "$TMP/ralph/ralph"
    cp "$REPO/ralph/prompt-build.md" "$TMP/ralph/prompt-build.md"
    printf '#!/usr/bin/env bash\nif [[ "$1" == rev-parse ]]; then printf "%%s\\n" main; fi\nexit 0\n' > "$TMP/fake-bin/git"
    chmod +x "$TMP/fake-bin/git"
    write_fake_pi "$TMP/fake-bin"
    : > "$TMP/pi-args.txt"
    run env PATH="$TMP/fake-bin:$PATH" PI_ARGS_LOG="$TMP/pi-args.txt" PI_CODING_AGENT_DIR="$TMP/pi-home" \
        PI_FAKE_REPLY='<promise>NEXT</promise>' "$TMP/ralph/ralph" --iterations 5 --budget 0.01
    [ "$status" -ne 0 ] || return 1
    [[ "$output" == *'session budget exceeded ($0.01 cap; actual $0.0200)'* ]] || return 1
    [ "$(grep -c 'Session usage' "$TMP/ralph/progress.txt")" -eq 1 ] || return 1
    # And the run-wide cap pauses before the next iteration starts.
    : > "$TMP/ralph/progress.txt"
    run env PATH="$TMP/fake-bin:$PATH" PI_ARGS_LOG="$TMP/pi-args.txt" PI_CODING_AGENT_DIR="$TMP/pi-home" \
        PI_FAKE_REPLY='<promise>NEXT</promise>' RALPH_BUDGET_RESERVE_USD=0.01 "$TMP/ralph/ralph" --iterations 5 --budget-total 0.025
    [ "$status" -ne 0 ] || return 1
    [[ "$output" == *'budget target nearly exhausted'* ]] || return 1
    [ "$(grep -c 'Session usage' "$TMP/ralph/progress.txt")" -eq 1 ] || return 1
}

# ── pi: background tools ──────────────────────────────────────────────────────
#
# bg_run returns before its command starts, so its child session begins after the
# tool result (measured 2026-09-02: result at 19.458s, child header at 19.801s). The
# task record pi-background-tasks writes under .pi/tasks carries startTime/endTime;
# without it the window closes at the task's terminal notification.

pi_bg_run_fixture() { # <call-ts> <result-ts> <child-ts>; appends a bg_run to $PI_MAIN and writes its child
  local sdir; sdir="$(dirname "$PI_MAIN")"
  {
    pi_msg "$1" assistant '[{"type":"toolCall","id":"b1","name":"bg_run","arguments":{"name":"child pi","command":"pi -p --model anthropic/claude-haiku-4-5 --thinking off \"PONG\"","isAgent":true}}]'
    pi_msg "$2" toolResult '[{"type":"text","text":"Started background task child pi (task-1)\nStatus: running\nPID: 4242\nOutput: .pi/tasks/main-0001-77/task-1.output"}]' '' '' ',"toolCallId":"b1","toolName":"bg_run","details":{"task":{"id":"task-1","name":"child pi","command":"pi -p --model anthropic/claude-haiku-4-5 --thinking off \"PONG\"","status":"running","pid":4242}}'
  } >> "$PI_MAIN"
  BG_CHILD="$sdir/$(printf '%s' "$3" | tr ':' '-' | sed 's/\.\([0-9]*\)Z$/-\1Z/')_bgrun-0001.jsonl"
  {
    printf '{"type":"session","version":3,"id":"bgrun-0001","timestamp":"%s","cwd":"%s"}\n' "$3" "$PI_CWD"
    pi_msg "$3" user '[{"type":"text","text":"PONG"}]'
    pi_msg "$3" assistant '[{"type":"text","text":"PONG"}]' claude-haiku-4-5 \
      '{"input":3,"output":6,"cacheRead":19733,"cacheWrite":329,"totalTokens":20071,"cost":{"input":0.000003,"output":0.00003,"cacheRead":0.0019733,"cacheWrite":0.00041125,"total":0.00241755}}'
  } > "$BG_CHILD"
}

@test "a bg_run child that starts after the tool result is matched by the task record's window" {
    pi_fixture
    pi_bg_run_fixture 2026-09-01T23:20:30.000Z 2026-09-01T23:20:30.100Z 2026-09-01T23:20:31.000Z
    local sdir; sdir="$(dirname "$PI_MAIN")"
    # A session after the task ended must not be claimed by it.
    {
      printf '{"type":"session","version":3,"id":"late-0001","timestamp":"2026-09-01T23:20:45.000Z","cwd":"%s"}\n' "$PI_CWD"
      pi_msg 2026-09-01T23:20:45.500Z assistant '[{"type":"text","text":"late"}]' claude-haiku-4-5 \
        '{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":3.0}}'
    } > "$sdir/2026-09-01T23-20-45-000Z_late-0001.jsonl"
    pi_msg 2026-09-01T23:20:50.000Z assistant '[{"type":"text","text":"<promise>NEXT</promise>"}]' >> "$PI_MAIN"
    mkdir -p "$PI_CWD/.pi/tasks/main-0001-77"
    # startTime/endTime are epoch milliseconds: 23:20:30.200Z .. 23:20:38.000Z
    printf '{"id":"task-1","status":"completed","pid":4242,"startTime":1788304830200,"endTime":1788304838000,"isAgent":true}\n' > "$PI_CWD/.pi/tasks/main-0001-77/task-1.json"
    run pi_transcript children "$PI_MAIN"
    [ "$status" -eq 0 ] || return 1
    [[ "$output" == *$'bg_run\tbgrun-0001\tclaude-haiku-4-5\t0.0024'* ]] || return 1
    [[ "$output" != *"late-0001"* ]] || return 1
    HARNESS=pi MODEL=m THINKING_TAG="" SESSION_SECS=1
    log_usage ralph-8 "$PI_STDOUT"
    grep -qF -- '- subagents (5) pi -p x2 · agent x1 · bg_run x1 · agent (no transcript) x1' "$PROGRESS" || return 1
    grep -qF -- '  - bg_run child pi: claude-haiku-4-5 $0.0024, 1 inference calls' "$PROGRESS" || return 1
}

@test "without a task record the bg_run window closes at the task's terminal notification" {
    pi_fixture
    pi_bg_run_fixture 2026-09-01T23:20:30.000Z 2026-09-01T23:20:30.100Z 2026-09-01T23:20:31.000Z
    local sdir; sdir="$(dirname "$PI_MAIN")"
    {
      printf '{"type":"session","version":3,"id":"late-0001","timestamp":"2026-09-01T23:20:45.000Z","cwd":"%s"}\n' "$PI_CWD"
      pi_msg 2026-09-01T23:20:45.500Z assistant '[{"type":"text","text":"late"}]' claude-haiku-4-5 \
        '{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":3.0}}'
    } > "$sdir/2026-09-01T23-20-45-000Z_late-0001.jsonl"
    printf '{"type":"custom_message","customType":"background-task-notification","content":"<background-task-notification>\\n  <task-id>task-1</task-id>\\n  <status>completed</status>\\n</background-task-notification>","id":"n1","parentId":"b1","timestamp":"2026-09-01T23:20:40.000Z"}\n' >> "$PI_MAIN"
    pi_msg 2026-09-01T23:20:50.000Z assistant '[{"type":"text","text":"<promise>NEXT</promise>"}]' >> "$PI_MAIN"
    run pi_transcript children "$PI_MAIN"
    [[ "$output" == *$'bg_run\tbgrun-0001'* ]] || return 1
    [[ "$output" != *"late-0001"* ]] || return 1
    # With neither record nor notification the window runs to the end of the
    # transcript, and the task still claims one session only: the earliest.
    grep -v 'background-task-notification' "$PI_MAIN" > "$PI_MAIN.tmp" && mv "$PI_MAIN.tmp" "$PI_MAIN"
    run pi_transcript children "$PI_MAIN"
    [[ "$output" == *$'bg_run\tbgrun-0001'* ]] || return 1
    [[ "$output" != *"late-0001"* ]] || return 1
}

@test "a bash child inside a bg_run's span still belongs to the bash call" {
    pi_fixture
    # The task spans 23:20:18 .. 23:20:38, which contains the PING bash child at 19.984.
    pi_bg_run_fixture 2026-09-01T23:20:18.000Z 2026-09-01T23:20:18.100Z 2026-09-01T23:20:18.500Z
    mkdir -p "$PI_CWD/.pi/tasks/main-0001-77"
    printf '{"id":"task-1","status":"completed","pid":4242,"startTime":1788304818100,"endTime":1788304838000}\n' > "$PI_CWD/.pi/tasks/main-0001-77/task-1.json"
    run pi_transcript children "$PI_MAIN"
    [[ "$output" == *$'pi -p\tshell-0001'* ]] || return 1
    [[ "$output" == *$'bg_run\tbgrun-0001'* ]] || return 1
    [[ "$output" != *$'bg_run\tshell-0001'* ]] || return 1
}

@test "a bg_delegate child is found by the session id and artifact dir its result names" {
    pi_fixture
    local art="$PI_CWD/.pi/delegate/main-0001-77/task-2"
    mkdir -p "$art/child-session"
    {
      printf '{"type":"session","version":3,"id":"delegate-abc123def456","timestamp":"2026-09-01T23:20:35.700Z","cwd":"%s"}\n' "$PI_CWD"
      pi_msg 2026-09-01T23:20:36.000Z assistant '[{"type":"text","text":"4"}]' claude-haiku-4-5 \
        '{"input":5,"output":40,"cacheRead":0,"cacheWrite":0,"totalTokens":45,"cost":{"input":0.000005,"output":0.0002,"cacheRead":0,"cacheWrite":0,"total":0.0080}}'
    } > "$art/child-session/2026-09-01T23-20-35-700Z_delegate-abc123def456.jsonl"
    {
      pi_msg 2026-09-01T23:20:35.000Z assistant '[{"type":"toolCall","id":"d1","name":"bg_delegate","arguments":{"name":"math","prompt":"2+2?"}}]'
      pi_msg 2026-09-01T23:20:35.200Z toolResult "[{\"type\":\"text\",\"text\":\"Started delegate math (task-2)\nRoute pinned: anthropic/claude-haiku-4-5\nChild session: delegate-abc123def456 (separate from this session)\nArtifacts: .pi/delegate/main-0001-77/task-2\"}]" '' '' ",\"toolCallId\":\"d1\",\"toolName\":\"bg_delegate\",\"details\":{\"task\":{\"id\":\"task-2\",\"command\":\"'pi' '--mode' 'text' '--print' '--session-id' 'delegate-abc123def456' '--session-dir' '$art/child-session' '--no-builtin-tools'\"}}"
    } >> "$PI_MAIN"
    run pi_transcript children "$PI_MAIN"
    [ "$status" -eq 0 ] || return 1
    [[ "$output" == *$'bg_delegate\tdelegate-abc123def456\tclaude-haiku-4-5\t0.0080'* ]] || return 1
    HARNESS=pi MODEL=m THINKING_TAG="" SESSION_SECS=1
    log_usage ralph-9 "$PI_STDOUT"
    grep -qF -- '  - bg_delegate 23def456: claude-haiku-4-5 $0.0080, 1 inference calls' "$PROGRESS" || return 1
    grep -qF -- '- cost $0.0786 (summed from 5 session transcripts' "$PROGRESS" || return 1
    # The result text alone, without the argv in details, is enough to find it.
    python3 - "$PI_MAIN" <<'PY'
import sys, re
p = sys.argv[1]
s = open(p).read().replace('"details":{"task":{"id":"task-2","command":"', '"details":{"task":{"id":"task-2","cmd_removed":"')
open(p, "w").write(s)
PY
    run pi_transcript children "$PI_MAIN"
    [[ "$output" == *$'bg_delegate\tdelegate-abc123def456'* ]] || return 1
}

# ── pi: the environment note and the prompts that depend on it ────────────────

@test "the environment note names the tools the harness actually has" {
    # env_note sits below the sourced region; load its real text.
    eval "$(sed -n '/^env_note() {/,/^}/p' "$RALPH")"
    HARNESS=pi
    run env_note
    [[ "$output" == *"chrome_open"* ]] || return 1
    [[ "$output" == *'bg_run'* ]] || return 1
    [[ "$output" == *'pi -p --model "$RALPH_MODEL" --thinking "$RALPH_THINKING" "$(cat /path/to/prompt.txt)"'* ]] || return 1
    [[ "$output" != *'< /path/to/prompt.txt'* ]] || return 1
    # Measured 2026-09-02: told by the task runner's own guideline to end the turn and
    # wait for the notification, the session exited and every background task died.
    [[ "$output" == *"ending your turn exits the process and kills every background task"* ]] || return 1
    [[ "$output" == *"Never end your turn to wait"* ]] || return 1
    # Measured 2026-09-02: a prompt's "600000 ms" became a seven-day bash timeout on pi.
    [[ "$output" == *"in SECONDS here, 600 at most"* ]] || return 1
    [[ "$output" != *"ToolSearch"* ]] || return 1
    [[ "$output" != *"Agent tool"* ]] || return 1
    HARNESS=claude
    run env_note
    [[ "$output" == *"ToolSearch"* ]] || return 1
    [[ "$output" == *"Agent tool"* ]] || return 1
    [[ "$output" != *"chrome_open"* ]] || return 1
    HARNESS=codex
    run env_note
    [[ "$output" == *"Agent tool"* ]] || return 1
}

@test "run_session hands a pi session the model and level its children must reuse" {
    fn=$(sed -n '/^run_session() {/,/^}/p' "$RALPH")
    [[ "$fn" == *'RALPH_MODEL="$MODEL" RALPH_THINKING="$THINKING"'* ]] || return 1
}

@test "pi takes the last SPOKEN assistant message as the result, past post-promise wake-ups" {
    # Measured 2026-09-02: 21 background tasks killed at exit each woke one empty
    # follow-up turn after <promise>COMPLETE</promise>; the last message read as no
    # promise and a finished pass was resumed.
    printf '%s\n' \
      '{"type":"session","version":3,"id":"wake-0001","timestamp":"2026-09-02T03:04:48.802Z","cwd":"/x"}' \
      '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"all pushed\n\n<promise>COMPLETE</promise>"}],"model":"m","stopReason":"stop"}}' \
      '{"type":"message_end","message":{"role":"assistant","content":[],"model":"m","stopReason":"stop"}}' \
      '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"   "}],"model":"m","stopReason":"stop"}}' \
      '{"type":"agent_end","messages":[{"role":"assistant","content":[],"model":"m","stopReason":"stop"}]}' > "$TMP/wake.json"
    normalize_pi_result "$TMP/wake.json" "$TMP/wake.meta"
    [ "$(promise_of "$(jsonfield "$TMP/wake.meta" result)")" = "COMPLETE" ] || return 1
    [ "$(jsonfield "$TMP/wake.meta" is_error)" = "False" ] || return 1
    # An error after the promise still counts: the iteration did not end cleanly.
    printf '%s\n' \
      '{"type":"session","version":3,"id":"wake-0002","timestamp":"2026-09-02T03:04:48.802Z","cwd":"/x"}' \
      '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"<promise>COMPLETE</promise>"}],"model":"m","stopReason":"stop"}}' \
      '{"type":"message_end","message":{"role":"assistant","content":[],"model":"m","stopReason":"error","errorMessage":"rate limited"}}' > "$TMP/wake2.json"
    normalize_pi_result "$TMP/wake2.json" "$TMP/wake2.meta"
    [ "$(jsonfield "$TMP/wake2.meta" is_error)" = "True" ] || return 1
}

@test "a pi session's tabs share exactly one Chrome window of its own" {
    [ -x "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" ] || skip "Chrome not installed"
    pi --offline --list-models claude-haiku-4-5 2>/dev/null | grep -q claude-haiku-4-5 || skip "haiku not available"
    # Two sessions, two tabs each: each session's tabs share one window, the sessions' differ.
    local prompt='Call chrome_open twice, both on "about:blank" (width 1000, height 700). Then chrome_eval "location.href" on each. Do not close them. End with <promise>NEXT</promise>'
    ( cd "$TMP" && printf '%s\n' "$prompt" | pi -p --mode json --model anthropic/claude-haiku-4-5 --thinking off > s1.json 2>/dev/null ) &
    ( cd "$TMP" && printf '%s\n' "$prompt" | pi -p --mode json --model anthropic/claude-haiku-4-5 --thinking off > s2.json 2>/dev/null ) &
    wait
    run node - <<'JS'
const info = await (await fetch("http://127.0.0.1:9222/json/version")).json();
const ws = new WebSocket(info.webSocketDebuggerUrl); await new Promise(r => ws.addEventListener("open", r, {once:true}));
let id=0; const pend=new Map(); ws.addEventListener("message", ev=>{const m=JSON.parse(ev.data); if(pend.has(m.id)){pend.get(m.id)(m.result||m.error); pend.delete(m.id);} });
const send=(method,params={})=>new Promise(res=>{const n=++id; pend.set(n,res); ws.send(JSON.stringify({id:n,method,params}));});
const {targetInfos}=await send("Target.getTargets"); const pages=targetInfos.filter(t=>t.type==="page" && t.url==="about:blank");
const byWin=new Map(); for (const t of pages) { const w=await send("Browser.getWindowForTarget",{targetId:t.targetId}); byWin.set(w.windowId,(byWin.get(w.windowId)||0)+1); }
console.log(JSON.stringify([...byWin.values()].sort()));
for (const t of pages) await send("Target.closeTarget",{targetId:t.targetId}); ws.close();
JS
    [ "$status" -eq 0 ] || return 1
    # Two windows holding two tabs each (a stale blank tab elsewhere would add a 1).
    [[ "$output" == *"2,2"* ]] || return 1
}
