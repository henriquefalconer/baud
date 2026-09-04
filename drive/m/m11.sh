#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/m/m11.sh — M11 drive script: real Msg::Frame persistence + real replay-based
# POST /runs/:id/stream/render, end to end against a real baud-server process on real /dev/kvm.
#
# todo.md §14's "eighteenth brick" left two gaps open: (1) no /run/kvm* boot route drained
# Msg::Frame records into frame_records at all — a real KVM boot's frames were captured
# in-process and immediately dropped; (2) POST /runs/:id/stream/render was an explicit stub that
# always fabricated a synthetic gradient from a stored hash, never the guest's real pixels. Both
# are closed by: POST /run/kvm gaining an optional run_id (persists kernel/cmdline/tape into
# kvm_run_meta plus every Msg::Frame it drains into frame_records), and stream::render replaying
# that exact kernel/cmdline/tape for real when kvm_run_meta has a row for the run.
#
#   M11.1 POST /run/kvm { run_id } with the framebuffer-guest fixture — real boot, frames_recorded=1.
#   M11.2 GET  /runs/:id/frames — the real frame row landed in the DB (width/height/format/hash).
#   M11.3 POST /runs/:id/stream/render (qoi-seq) — real replay path, decodes to the guest's *actual*
#         pixels (10,10,10),(20,20,20),(30,30,30),(40,40,40) — not a hash-seeded synthetic gradient.
#   M11.4 Re-rendering the same run reproduces byte-identical output (real replay is deterministic).
#   M11.5 The pre-pivot synthetic fallback still works for a run with no kvm_run_meta row (manually
#         seeded via POST /runs/:id/frames, exactly as drive/m/m5.sh does) — no regression.

set -euo pipefail

cd "$(dirname "$0")/../.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

REPO_ROOT="$(pwd)"
BAUD="$REPO_ROOT/target/debug/baud"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
SERVER_PID=""
DB_FILE="$(mktemp -t baud-m11-XXXXXX.sqlite)"
DB_FILE="$(cygpath -m "$DB_FILE" 2>/dev/null || echo "$DB_FILE")"
OUT_QOI="$(mktemp -t baud-m11-out-XXXXXX.qoi)"
OUT_QOI2="$(mktemp -t baud-m11-out2-XXXXXX.qoi)"
# M11.5's synthetic-fallback render needs an explicit "out" too: stream::render defaults to the
# *relative* path "output.y4m" (crates/baud-server/src/routes/stream.rs), which would both pollute
# the repo root and collide between two concurrent runs of this script.
OUT_LEGACY="$(mktemp -t baud-m11-legacy-XXXXXX.qoi)"
SNAP_ROOT="$(mktemp -d -t baud-m11-snap-XXXXXX)"

# Ephemeral port + per-script snapshot store, so this script can run concurrently with any other
# drive/*.sh (each server gets its own port, its own SQLite file and its own SnapshotStore root).
BAUD_PORT="${BAUD_PORT:-$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));p=s.getsockname()[1];s.close();print(p)')}"
SRV="http://127.0.0.1:$BAUD_PORT"
export BAUD_SERVER="$SRV"

cleanup() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    sleep 0.2
    rm -f "$DB_FILE" "$OUT_QOI" "$OUT_QOI2" "$OUT_LEGACY" 2>/dev/null || true
    rm -rf "$SNAP_ROOT" 2>/dev/null || true
}
trap cleanup EXIT
# bash does NOT run an EXIT trap when it dies from an untrapped signal, so `trap cleanup EXIT`
# alone leaks this script's baud-server, temp DB and snapshot dir whenever the script is
# interrupted -- Ctrl-C, or drive/gate.sh reaping its pool. Re-raising through `exit` makes the
# EXIT trap fire, so cleanup() runs on every exit path. (This is how 21 stray temp SQLite files
# and two orphaned servers survived a killed gate run.)
trap 'exit 130' INT
trap 'exit 143' TERM

log() { echo "[m11] $*" >&2; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ $*" >&2; exit 1; }

FRAMEBUFFER_GUEST_KERNEL="$REPO_ROOT/crates/baud-multiverse/tests/fixtures/framebuffer-guest/bzImage"
[[ -f "$FRAMEBUFFER_GUEST_KERNEL" ]] || fail "fixture kernel missing: $FRAMEBUFFER_GUEST_KERNEL"

log "Building baud-server and baud..."
if [[ -z "${BAUD_GATE_PREBUILT:-}" ]]; then
    cargo build -q --bin baud-server --bin baud 2>&1
fi

log "Starting baud-server (DB: $DB_FILE, port: $BAUD_PORT)..."
BAUD_DB="sqlite://${DB_FILE}?mode=rwc" BAUD_ADDR="127.0.0.1:$BAUD_PORT" \
    BAUD_SNAPSHOT_STORE="$SNAP_ROOT" BAUD_LOG=warn "$BAUD_SERVER_BIN" &
SERVER_PID=$!

for i in $(seq 1 60); do
    if curl -sf "$SRV/health" > /dev/null 2>&1; then
        break
    fi
    sleep 0.2
done
curl -sf "$SRV/health" > /dev/null || fail "baud-server did not start"
pass "baud-server is running (PID $SERVER_PID)"

# ---------------------------------------------------------------------------
# M11.1 — POST /run/kvm with run_id, real boot, real frame drained
# ---------------------------------------------------------------------------
log "--- M11.1: POST /run/kvm { run_id } — framebuffer-guest, real Frame drain ---"
RUN_ID="m11-real-frame-$$"
BOOT=$(curl -sf -X POST "$SRV/run/kvm" -H "Content-Type: application/json" \
    -d "{\"kernel_path\": \"$FRAMEBUFFER_GUEST_KERNEL\", \"run_id\": \"$RUN_ID\"}")
BOOT_OK=$(echo "$BOOT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$BOOT_OK" == "True" ]] || fail "M11.1: /run/kvm returned ok!=true: $BOOT"
FRAMES_RECORDED=$(echo "$BOOT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('frames_recorded', -1))")
[[ "$FRAMES_RECORDED" == "1" ]] || fail "M11.1: expected frames_recorded=1, got $FRAMES_RECORDED: $BOOT"
pass "M11.1: /run/kvm booted framebuffer-guest and persisted 1 real Frame record under run_id=$RUN_ID"

# ---------------------------------------------------------------------------
# M11.2 — GET /runs/:id/frames — the real row landed in the DB
# ---------------------------------------------------------------------------
log "--- M11.2: GET /runs/:id/frames — real frame row in frame_records ---"
FRAMES=$(curl -sf "$SRV/runs/$RUN_ID/frames")
FRAME_COUNT=$(echo "$FRAMES" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['frames']))")
[[ "$FRAME_COUNT" == "1" ]] || fail "M11.2: expected 1 frame row, got $FRAME_COUNT: $FRAMES"
FRAME_W=$(echo "$FRAMES" | python3 -c "import sys,json; print(json.load(sys.stdin)['frames'][0]['width'])")
FRAME_H=$(echo "$FRAMES" | python3 -c "import sys,json; print(json.load(sys.stdin)['frames'][0]['height'])")
FRAME_FMT=$(echo "$FRAMES" | python3 -c "import sys,json; print(json.load(sys.stdin)['frames'][0]['format'])")
[[ "$FRAME_W" == "2" && "$FRAME_H" == "2" && "$FRAME_FMT" == "indexed8" ]] \
    || fail "M11.2: unexpected frame geometry/format: w=$FRAME_W h=$FRAME_H fmt=$FRAME_FMT"
pass "M11.2: real frame row persisted (2x2 indexed8)"

# ---------------------------------------------------------------------------
# M11.3 — POST /runs/:id/stream/render — real replay, decodes to the guest's actual pixels
# ---------------------------------------------------------------------------
log "--- M11.3: POST /runs/:id/stream/render — real replay path ---"
RENDER=$(curl -sf -X POST "$SRV/runs/$RUN_ID/stream/render" -H "Content-Type: application/json" \
    -d "{\"format\": \"qoi-seq\", \"out\": \"$OUT_QOI\"}")
RENDER_OK=$(echo "$RENDER" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$RENDER_OK" == "True" ]] || fail "M11.3: render returned ok!=true: $RENDER"
RENDER_FRAMES=$(echo "$RENDER" | python3 -c "import sys,json; print(json.load(sys.stdin)['frame_count'])")
[[ "$RENDER_FRAMES" == "1" ]] || fail "M11.3: expected frame_count=1, got $RENDER_FRAMES: $RENDER"
[[ -s "$OUT_QOI" ]] || fail "M11.3: render did not write a non-empty output file"

DECODED_OK=$(python3 - "$OUT_QOI" <<'PYEOF'
import sys

def decode_qoi(path):
    data = open(path, "rb").read()
    assert data[:4] == b"qoif", "bad QOI magic"
    width = int.from_bytes(data[4:8], "big")
    height = int.from_bytes(data[8:12], "big")
    pos = 14
    seen = [(0, 0, 0, 0)] * 64
    prev = (0, 0, 0, 255)
    pixels = []
    n = width * height
    while len(pixels) < n:
        b0 = data[pos]
        if b0 == 0xFE:
            r, g, b = data[pos + 1], data[pos + 2], data[pos + 3]
            a = prev[3]
            pos += 4
        elif b0 == 0xFF:
            r, g, b, a = data[pos + 1], data[pos + 2], data[pos + 3], data[pos + 4]
            pos += 5
        else:
            tag = b0 & 0xC0
            if tag == 0x00:
                r, g, b, a = seen[b0 & 0x3F]
                pos += 1
            elif tag == 0x40:
                dr = ((b0 >> 4) & 0x3) - 2
                dg = ((b0 >> 2) & 0x3) - 2
                db = (b0 & 0x3) - 2
                r = (prev[0] + dr) & 0xFF
                g = (prev[1] + dg) & 0xFF
                b = (prev[2] + db) & 0xFF
                a = prev[3]
                pos += 1
            elif tag == 0x80:
                b1 = data[pos + 1]
                dg = (b0 & 0x3F) - 32
                dr_dg = ((b1 >> 4) & 0xF) - 8
                db_dg = (b1 & 0xF) - 8
                g = (prev[1] + dg) & 0xFF
                r = (prev[0] + dg + dr_dg) & 0xFF
                b = (prev[2] + dg + db_dg) & 0xFF
                a = prev[3]
                pos += 2
            else:
                run = (b0 & 0x3F) + 1
                for _ in range(run):
                    pixels.append(prev)
                pos += 1
                continue
        px = (r, g, b, a)
        pixels.append(px)
        idx = (r * 3 + g * 5 + b * 7 + a * 11) % 64
        seen[idx] = px
        prev = px
    return width, height, pixels

w, h, pixels = decode_qoi(sys.argv[1])
expected = [(10, 10, 10, 255), (20, 20, 20, 255), (30, 30, 30, 255), (40, 40, 40, 255)]
if (w, h) != (2, 2):
    print(f"BAD_GEOMETRY {w}x{h}")
elif pixels != expected:
    print(f"BAD_PIXELS {pixels}")
else:
    print("OK")
PYEOF
)
[[ "$DECODED_OK" == "OK" ]] || fail "M11.3: decoded QOI pixels are not the guest's real pixels: $DECODED_OK"
pass "M11.3: rendered QOI decodes to the guest's real pixels (10,10,10),(20,20,20),(30,30,30),(40,40,40) — not a synthetic gradient"

# ---------------------------------------------------------------------------
# M11.4 — re-rendering reproduces byte-identically (real replay is deterministic)
# ---------------------------------------------------------------------------
log "--- M11.4: re-render is byte-identical (real replay determinism) ---"
curl -sf -X POST "$SRV/runs/$RUN_ID/stream/render" -H "Content-Type: application/json" \
    -d "{\"format\": \"qoi-seq\", \"out\": \"$OUT_QOI2\"}" > /dev/null
cmp -s "$OUT_QOI" "$OUT_QOI2" || fail "M11.4: re-render produced different bytes — real replay must be deterministic"
pass "M11.4: re-rendering the same run reproduces byte-identical output"

# ---------------------------------------------------------------------------
# M11.5 — hash-only records fail closed because hashes cannot be rendered into pixels
# ---------------------------------------------------------------------------
log "--- M11.5: hash-only render is rejected without replay metadata ---"
LEGACY_RUN_OUT=$(BAUD_SERVER="$SRV" "$BAUD" run start \
    --spec examples/framedemo/spec.yaml \
    --seed 101 \
    --json 2>&1)
LEGACY_RUN_ID=$(echo "$LEGACY_RUN_OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('id', ''))")
[[ -n "$LEGACY_RUN_ID" ]] || fail "M11.5: run start did not return an id: $LEGACY_RUN_OUT"
HASH=$(python3 -c "import hashlib; print(hashlib.blake2b(b'm11-legacy-frame', digest_size=32).hexdigest())")
curl -sf -X POST "$SRV/runs/$LEGACY_RUN_ID/frames" -H "Content-Type: application/json" \
    -d "{\"node\": 0, \"step\": 0, \"width\": 4, \"height\": 4, \"format\": \"indexed8\", \"hash\": \"$HASH\"}" > /dev/null
LEGACY_RENDER=$(curl -sf -X POST "$SRV/runs/$LEGACY_RUN_ID/stream/render" -H "Content-Type: application/json" \
    -d "{\"format\": \"qoi-seq\", \"out\": \"$OUT_LEGACY\"}")
LEGACY_ERROR=$(echo "$LEGACY_RENDER" | python3 -c "import sys,json; print(json.load(sys.stdin).get('error', ''))")
echo "$LEGACY_ERROR" | grep -q "no replayable KVM image" || fail "M11.5: hash-only render was not rejected: $LEGACY_RENDER"
pass "M11.5: pre-pivot hash-only frames fail closed instead of fabricating pixels"

curl -sf "$SRV/health" > /dev/null || fail "baud-server is no longer healthy at the end of the run"
pass "baud-server remained healthy throughout"

echo ""
echo "==========================================="
echo "ALL M11 CHECKS PASSED"
echo "==========================================="
echo ""
echo "Real Msg::Frame persistence + real replay-based POST /runs/:id/stream/render exercised"
echo "end-to-end against a real baud-server process on real /dev/kvm:"
echo "  POST /run/kvm { run_id }     — real boot persists kernel/cmdline/tape + real Frame records"
echo "  GET  /runs/:id/frames        — real frame row in the DB"
echo "  POST /runs/:id/stream/render — real replay decodes to the guest's actual pixels"
echo "  determinism                  — re-rendering reproduces byte-identical output"
echo "  fail-closed                  — hash-only manually-seeded runs are rejected without replay metadata"
