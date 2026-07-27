#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/m/m2.sh — M2 drive script: provisioning
#
# Validates:
#   baud spec new <name>        → generates a template
#   baud spec lint <path>       → lints a valid spec
#   baud spec lint <bad-spec>   → exits 1 on bad spec
#   baud spec show <path>       → shows parsed spec as JSON
#   baud run start --spec <path> → starts a run
#   baud run ls                 → lists runs
#   baud run status <id>        → shows run status with closure hash
#   baud obs ls --run <id>      → shows (empty) observations

set -euo pipefail

cd "$(dirname "$0")/../.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

REPO_ROOT="$(pwd)"
BAUD="$REPO_ROOT/target/debug/baud"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
SERVER_PID=""
DB_FILE="$(mktemp -t baud-m2-XXXXXX.sqlite)"
# Windows/git-bash: sqlite:// URIs need a native Windows path (posix /tmp/... is not
# understood by a plain win32 binary); cygpath -m gives a forward-slash Windows path.
DB_FILE="$(cygpath -m "$DB_FILE" 2>/dev/null || echo "$DB_FILE")"
TMPDIR_WORK="$(mktemp -d -t baud-m2-work.XXXXXX)"

cleanup() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
    fi
    sleep 0.2
    rm -f "$DB_FILE" 2>/dev/null || true
    rm -rf "$TMPDIR_WORK"
}
trap cleanup EXIT

log() { echo "[m2] $*" >&2; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ $*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------
log "Building workspace..."
cargo build -q --bin baud-server --bin baud 2>&1

# ---------------------------------------------------------------------------
# Start baud-server
# ---------------------------------------------------------------------------
log "Starting baud-server (DB: $DB_FILE)..."
pkill -f "baud-server" 2>/dev/null || true; sleep 0.2
BAUD_DB="sqlite://${DB_FILE}?mode=rwc" BAUD_LOG=warn \
    "$BAUD_SERVER_BIN" &
SERVER_PID=$!

for i in $(seq 1 30); do
    if curl -sf http://127.0.0.1:7734/health > /dev/null 2>&1; then
        break
    fi
    sleep 0.2
done
curl -sf http://127.0.0.1:7734/health > /dev/null || fail "baud-server did not start"
pass "baud-server is running"

# ---------------------------------------------------------------------------
# M2.1 — spec new: generate a template
# ---------------------------------------------------------------------------
log "--- M2.1: spec new ---"
SPEC_NEW_PATH="$TMPDIR_WORK/hello-new.yaml"
BAUD_SERVER=http://127.0.0.1:7734 $BAUD spec new hello-new --output "$SPEC_NEW_PATH" --json > /dev/null
[[ -f "$SPEC_NEW_PATH" ]] || fail "spec new: output file not created"
grep -q "nix:" "$SPEC_NEW_PATH" || fail "spec new: template missing 'nix:' directive"
pass "spec new: template created at $SPEC_NEW_PATH"

# ---------------------------------------------------------------------------
# M2.2 — spec lint: valid spec
# ---------------------------------------------------------------------------
log "--- M2.2: spec lint (valid) ---"
LINT_OUT=$(BAUD_SERVER=http://127.0.0.1:7734 $BAUD spec lint examples/hello-deterministic/spec.yaml --json 2>&1)
LINT_OK=$(echo "$LINT_OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$LINT_OK" == "True" ]] || fail "spec lint (valid): expected ok=true, got: $LINT_OUT"
NIX_REF=$(echo "$LINT_OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('nix',''))")
[[ -n "$NIX_REF" ]] || fail "spec lint: missing nix_ref in response"
pass "spec lint (valid): ok=true, nix_ref=$NIX_REF"

# ---------------------------------------------------------------------------
# M2.3 — spec lint: invalid spec (unknown directive) → exit 1
# ---------------------------------------------------------------------------
log "--- M2.3: spec lint (invalid) ---"
BAD_SPEC="$TMPDIR_WORK/bad.yaml"
cat > "$BAD_SPEC" << 'EOF'
nix: "./flake.nix#foo"
bogus_directive: true
nodes:
  - name: n0
    argv: ["foo"]
EOF
LINT_BAD_OUT=$(BAUD_SERVER=http://127.0.0.1:7734 $BAUD spec lint "$BAD_SPEC" --json 2>&1 || true)
LINT_BAD_OK=$(echo "$LINT_BAD_OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', True))")
[[ "$LINT_BAD_OK" == "False" ]] || fail "spec lint (invalid): expected ok=false for unknown directive, got: $LINT_BAD_OUT"
pass "spec lint (invalid): correctly rejected unknown directive"

# ---------------------------------------------------------------------------
# M2.4 — spec lint: invalid adapter → exit 1
# ---------------------------------------------------------------------------
log "--- M2.4: spec lint (bad adapter) ---"
BAD_ADAPTER_SPEC="$TMPDIR_WORK/bad-adapter.yaml"
cat > "$BAD_ADAPTER_SPEC" << 'EOF'
nix: "./flake.nix#foo"
nodes:
  - name: n0
    argv: ["foo"]
    adapters:
      input: exec-hook
EOF
LINT_ADAPTER_OUT=$(BAUD_SERVER=http://127.0.0.1:7734 $BAUD spec lint "$BAD_ADAPTER_SPEC" --json 2>&1 || true)
LINT_ADAPTER_OK=$(echo "$LINT_ADAPTER_OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', True))")
[[ "$LINT_ADAPTER_OK" == "False" ]] || fail "spec lint (bad adapter): expected ok=false, got: $LINT_ADAPTER_OUT"
pass "spec lint (bad adapter): correctly rejected unknown adapter"

# ---------------------------------------------------------------------------
# M2.5 — spec show: parsed spec as JSON
# ---------------------------------------------------------------------------
log "--- M2.5: spec show ---"
SHOW_OUT=$(BAUD_SERVER=http://127.0.0.1:7734 $BAUD spec show examples/hello-deterministic/spec.yaml --json 2>&1)
SHOW_NIX=$(echo "$SHOW_OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('nix',''))")
[[ -n "$SHOW_NIX" ]] || fail "spec show: missing nix in output: $SHOW_OUT"
SHOW_NODES=$(echo "$SHOW_OUT" | python3 -c "import sys,json; print(len(json.load(sys.stdin).get('nodes',[])))")
[[ "$SHOW_NODES" == "1" ]] || fail "spec show: expected 1 node, got $SHOW_NODES"
pass "spec show: nix=$SHOW_NIX, nodes=$SHOW_NODES"

# ---------------------------------------------------------------------------
# M2.6 — run start: provisions a run
# ---------------------------------------------------------------------------
log "--- M2.6: run start ---"
RUN_OUT=$(BAUD_SERVER=http://127.0.0.1:7734 $BAUD run start \
    --spec examples/hello-deterministic/spec.yaml \
    --seed 42 \
    --budget-minutes 60 \
    --json 2>&1)
RUN_ID=$(echo "$RUN_OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('id',''))")
[[ -n "$RUN_ID" ]] || fail "run start: missing id in response: $RUN_OUT"
RUN_SPEC_HASH=$(echo "$RUN_OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('spec_hash',''))")
[[ -n "$RUN_SPEC_HASH" ]] || fail "run start: missing spec_hash in response"
RUN_CLOSURE_HASH=$(echo "$RUN_OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('closure_hash',''))")
[[ -n "$RUN_CLOSURE_HASH" ]] || fail "run start: missing closure_hash in response"
pass "run start: id=$RUN_ID spec_hash=${RUN_SPEC_HASH:0:16}... closure_hash=${RUN_CLOSURE_HASH:0:16}..."

# ---------------------------------------------------------------------------
# M2.7 — run ls: shows the run
# ---------------------------------------------------------------------------
log "--- M2.7: run ls ---"
LS_OUT=$(BAUD_SERVER=http://127.0.0.1:7734 $BAUD run ls --json 2>&1)
FOUND_RUN=$(echo "$LS_OUT" | python3 -c "
import sys,json
d = json.load(sys.stdin)
runs = d.get('runs', [])
print('yes' if any(r['id'] == '$RUN_ID' for r in runs) else 'no')
")
[[ "$FOUND_RUN" == "yes" ]] || fail "run ls: run $RUN_ID not found in listing"
pass "run ls: found run $RUN_ID"

# ---------------------------------------------------------------------------
# M2.8 — run status: shows closure hash
# ---------------------------------------------------------------------------
log "--- M2.8: run status ---"
sleep 0.3  # Wait for provisioning background task
STATUS_OUT=$(BAUD_SERVER=http://127.0.0.1:7734 $BAUD run status "$RUN_ID" --json 2>&1)
STATUS_CLOSURE=$(echo "$STATUS_OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('closure_hash',''))")
[[ -n "$STATUS_CLOSURE" ]] || fail "run status: missing closure_hash in response"
STATUS_STATUS=$(echo "$STATUS_OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('status',''))")
[[ -n "$STATUS_STATUS" ]] || fail "run status: missing status field"
pass "run status: status=$STATUS_STATUS, closure_hash=${STATUS_CLOSURE:0:16}..."

# ---------------------------------------------------------------------------
# M2.9 — obs ls: shows (empty) observations
# ---------------------------------------------------------------------------
log "--- M2.9: obs ls ---"
OBS_OUT=$(BAUD_SERVER=http://127.0.0.1:7734 $BAUD obs ls --run "$RUN_ID" --json 2>&1)
OBS_LIST=$(echo "$OBS_OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('observations','ERROR'))")
# Observations is empty at this stage (M3 fills it in)
[[ "$OBS_LIST" == "[]" ]] || fail "obs ls: expected empty observations list, got: $OBS_LIST"
pass "obs ls: observations=[] (correct for pre-M3)"

# ---------------------------------------------------------------------------
# M2.10 — workload-noun CI grep (crates/baud-*/src must not contain workload names)
# ---------------------------------------------------------------------------
log "--- M2.10: workload-noun CI grep ---"
if grep -rn --include="*.rs" -E "\b(mario|raftlet|emulator|joypad)\b|\bnes\b" \
    $(ls -d crates/baud-*/src/ 2>/dev/null | grep -v "crates/baud-raftlet/") 2>/dev/null | grep -v "^$"; then
    fail "workload noun found in infra crates — CI grep FAILED"
fi
pass "workload-noun grep: CLEAN"

# ---------------------------------------------------------------------------
# M2.11 — spec lint (raftlet multi-node spec)
# ---------------------------------------------------------------------------
log "--- M2.11: spec lint (raftlet multi-node) ---"
RAFTLET_SPEC="$TMPDIR_WORK/raftlet.yaml"
cat > "$RAFTLET_SPEC" << 'EOF'
nix: "./flake.nix#raftlet"
env:
  RUST_BACKTRACE: "0"
nodes:
  - name: n0
    argv: ["raftlet", "--id", "0"]
    adapters:
      input: net
      probes:
        - stdout-kv
  - name: n1
    argv: ["raftlet", "--id", "1"]
    adapters:
      input: net
      probes:
        - stdout-kv
  - name: n2
    argv: ["raftlet", "--id", "2"]
    adapters:
      input: net
      probes:
        - stdout-kv
EOF
RAFTLET_OUT=$(BAUD_SERVER=http://127.0.0.1:7734 $BAUD spec lint "$RAFTLET_SPEC" --json 2>&1)
RAFTLET_OK=$(echo "$RAFTLET_OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
RAFTLET_NODES=$(echo "$RAFTLET_OUT" | python3 -c "import sys,json; print(len(json.load(sys.stdin).get('nodes', [])))")
[[ "$RAFTLET_OK" == "True" ]] || fail "spec lint (raftlet): expected ok=true, got: $RAFTLET_OUT"
[[ "$RAFTLET_NODES" == "3" ]] || fail "spec lint (raftlet): expected 3 nodes, got $RAFTLET_NODES"
pass "spec lint (raftlet): ok=true, nodes=3"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "M2 milestone: ALL CHECKS PASSED"
echo ""
echo "New crates:"
echo "  baud-init     — YAML spec parser + adapter schema (5 directives, closed adapter set)"
echo "  baud-packages — spec.toml → pinned flake → closure hash"
echo ""
echo "New functionality:"
echo "  baud spec new/lint/show"
echo "  baud run start/ls/status"
echo "  baud obs ls"
echo "  Run records in SQLite with spec_hash and closure_hash"
