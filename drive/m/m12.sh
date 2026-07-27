#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/m/m12.sh — M12 drive script: a branch-originated run also gets a kvm_run_meta row, end to
# end against a real baud-server process on real /dev/kvm.
#
# todo.md §14 item 1's newly-surfaced gap (closed by this): only plain POST /run/kvm { run_id }
# ever called persist_kvm_run — POST /run/kvm/branch (and /run/kvm/resume) never did, so a real
# guest's frames from a branch-forked run could never be replayed by POST /runs/:id/stream/render
# (it would silently fall back to the synthetic-gradient path, or find nothing at all). Closed by
# threading each branch's drained tape-device records back to the HTTP handler and reusing the same
# persist_kvm_run() /run/kvm already calls, keyed by a new optional RunKvmBranchBody.frame_run_ids
# (one run id per branch_tapes_hex entry). This works because boot_and_snapshot always snapshots
# the shared branch point with an *empty* tape, before any guest instruction runs — so a branch's
# own suffix is its entire replay tape from a cold boot, byte-identical to forking from the
# snapshot (proved directly by M12.3 below: real-replay pixels match the fixture's own direct
# /run/kvm output from drive/m/m11.sh).
#
#   M12.1 POST /run/kvm/branch { branch_tapes_hex: [""], frame_run_ids: [id] } with the
#         framebuffer-guest fixture — real boot+branch, frame_persistence[0].frames_recorded=1.
#   M12.2 GET  /runs/:id/frames — the real frame row landed in the DB (width/height/format).
#   M12.3 POST /runs/:id/stream/render (qoi-seq) — real replay path, decodes to the guest's *actual*
#         pixels (10,10,10),(20,20,20),(30,30,30),(40,40,40) — the same real pixels drive/m/m11.sh's
#         plain /run/kvm path proves, now reached via a branch-originated run instead.
#   M12.4 Re-rendering the same run reproduces byte-identical output (real replay is deterministic).

set -euo pipefail

cd "$(dirname "$0")/../.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

REPO_ROOT="$(pwd)"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
SERVER_PID=""
DB_FILE="$(mktemp -t baud-m12-XXXXXX.sqlite)"
DB_FILE="$(cygpath -m "$DB_FILE" 2>/dev/null || echo "$DB_FILE")"
OUT_QOI="$(mktemp -t baud-m12-out-XXXXXX.qoi)"
OUT_QOI2="$(mktemp -t baud-m12-out2-XXXXXX.qoi)"
SNAP_ROOT="$(mktemp -d -t baud-m12-snap-XXXXXX)"

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
    rm -f "$DB_FILE" "$OUT_QOI" "$OUT_QOI2" 2>/dev/null || true
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

log() { echo "[m12] $*" >&2; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ $*" >&2; exit 1; }

FRAMEBUFFER_GUEST_KERNEL="$REPO_ROOT/crates/baud-multiverse/tests/fixtures/framebuffer-guest/bzImage"
[[ -f "$FRAMEBUFFER_GUEST_KERNEL" ]] || fail "fixture kernel missing: $FRAMEBUFFER_GUEST_KERNEL"

log "Building baud-server..."
if [[ -z "${BAUD_GATE_PREBUILT:-}" ]]; then
    cargo build -q --bin baud-server 2>&1
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
# M12.1 — POST /run/kvm/branch { branch_tapes_hex: [""], frame_run_ids: [id] }
# ---------------------------------------------------------------------------
log "--- M12.1: POST /run/kvm/branch { frame_run_ids } — framebuffer-guest, real Frame drain ---"
RUN_ID="m12-branch-frame-$$"
BOOT=$(curl -sf -X POST "$SRV/run/kvm/branch" -H "Content-Type: application/json" \
    -d "{\"kernel_path\": \"$FRAMEBUFFER_GUEST_KERNEL\", \"branch_tapes_hex\": [\"\"], \"frame_run_ids\": [\"$RUN_ID\"]}")
BOOT_OK=$(echo "$BOOT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$BOOT_OK" == "True" ]] || fail "M12.1: /run/kvm/branch returned ok!=true: $BOOT"
FRAMES_RECORDED=$(echo "$BOOT" | python3 -c "import sys,json; print(json.load(sys.stdin)['frame_persistence'][0]['frames_recorded'])")
[[ "$FRAMES_RECORDED" == "1" ]] || fail "M12.1: expected frame_persistence[0].frames_recorded=1, got $FRAMES_RECORDED: $BOOT"
pass "M12.1: /run/kvm/branch booted+branched framebuffer-guest and persisted 1 real Frame record under run_id=$RUN_ID"

# ---------------------------------------------------------------------------
# M12.2 — GET /runs/:id/frames — the real row landed in the DB
# ---------------------------------------------------------------------------
log "--- M12.2: GET /runs/:id/frames — real frame row in frame_records ---"
FRAMES=$(curl -sf "$SRV/runs/$RUN_ID/frames")
FRAME_COUNT=$(echo "$FRAMES" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['frames']))")
[[ "$FRAME_COUNT" == "1" ]] || fail "M12.2: expected 1 frame row, got $FRAME_COUNT: $FRAMES"
FRAME_W=$(echo "$FRAMES" | python3 -c "import sys,json; print(json.load(sys.stdin)['frames'][0]['width'])")
FRAME_H=$(echo "$FRAMES" | python3 -c "import sys,json; print(json.load(sys.stdin)['frames'][0]['height'])")
FRAME_FMT=$(echo "$FRAMES" | python3 -c "import sys,json; print(json.load(sys.stdin)['frames'][0]['format'])")
[[ "$FRAME_W" == "2" && "$FRAME_H" == "2" && "$FRAME_FMT" == "indexed8" ]] \
    || fail "M12.2: unexpected frame geometry/format: w=$FRAME_W h=$FRAME_H fmt=$FRAME_FMT"
pass "M12.2: real frame row persisted (2x2 indexed8)"

# ---------------------------------------------------------------------------
# M12.3 — POST /runs/:id/stream/render — real replay, decodes to the guest's actual pixels
# ---------------------------------------------------------------------------
log "--- M12.3: POST /runs/:id/stream/render — real replay path ---"
RENDER=$(curl -sf -X POST "$SRV/runs/$RUN_ID/stream/render" -H "Content-Type: application/json" \
    -d "{\"format\": \"qoi-seq\", \"out\": \"$OUT_QOI\"}")
RENDER_OK=$(echo "$RENDER" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$RENDER_OK" == "True" ]] || fail "M12.3: render returned ok!=true: $RENDER"
RENDER_FRAMES=$(echo "$RENDER" | python3 -c "import sys,json; print(json.load(sys.stdin)['frame_count'])")
[[ "$RENDER_FRAMES" == "1" ]] || fail "M12.3: expected frame_count=1, got $RENDER_FRAMES: $RENDER"
[[ -s "$OUT_QOI" ]] || fail "M12.3: render did not write a non-empty output file"

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
[[ "$DECODED_OK" == "OK" ]] || fail "M12.3: decoded QOI pixels are not the guest's real pixels: $DECODED_OK"
pass "M12.3: rendered QOI decodes to the guest's real pixels (10,10,10),(20,20,20),(30,30,30),(40,40,40) — a branch-originated run replays for real, not synthetically"

# ---------------------------------------------------------------------------
# M12.4 — re-rendering reproduces byte-identically (real replay is deterministic)
# ---------------------------------------------------------------------------
log "--- M12.4: re-render is byte-identical (real replay determinism) ---"
curl -sf -X POST "$SRV/runs/$RUN_ID/stream/render" -H "Content-Type: application/json" \
    -d "{\"format\": \"qoi-seq\", \"out\": \"$OUT_QOI2\"}" > /dev/null
cmp -s "$OUT_QOI" "$OUT_QOI2" || fail "M12.4: re-render produced different bytes — real replay must be deterministic"
pass "M12.4: re-rendering the same run reproduces byte-identical output"

curl -sf "$SRV/health" > /dev/null || fail "baud-server is no longer healthy at the end of the run"
pass "baud-server remained healthy throughout"

echo ""
echo "==========================================="
echo "ALL M12 CHECKS PASSED"
echo "==========================================="
echo ""
echo "A branch-originated run also gets a kvm_run_meta row, exercised end-to-end against a real"
echo "baud-server process on real /dev/kvm:"
echo "  POST /run/kvm/branch { frame_run_ids } — real boot+branch persists a real Frame record"
echo "  GET  /runs/:id/frames                  — real frame row in the DB"
echo "  POST /runs/:id/stream/render           — real replay decodes to the guest's actual pixels"
echo "  determinism                            — re-rendering reproduces byte-identical output"
