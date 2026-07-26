#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/m13.sh — M13 drive script: a resume-originated run also gets a real, replayable
# kvm_run_meta row, end to end against a real baud-server process on real /dev/kvm.
#
# todo.md §14 item 1's "/run/kvm/resume's lineage gap" (closed by this): unlike a branch-originated
# run (drive/m12.sh), /run/kvm/resume never boots a kernel at all — it restores a Universe straight
# out of SnapshotStore. So its frames can't be reproduced by rebooting kernel_path+tape_hex the way
# stream::render's existing real-replay path does; they need a *restore*-and-replay instead:
# reconstruct the same Universe from the store and re-fork it with the same tape suffix via
# Multiverse::branch. kvm_run_meta gained nullable store_run_id/snapshot_node_id columns
# (migration 0012) for exactly this, and RunKvmResumeBody::frame_run_ids (mirroring
# RunKvmBranchBody::frame_run_ids) persists a restore-based row instead of a reboot-based one.
#
#   M13.1 POST /run/kvm/branch { branch_tapes_hex: [], persist_run_id } — persist-only mode: boot
#         the framebuffer-guest fixture, snapshot it immediately after boot (before it runs at all),
#         persist the Universe, fork nothing yet.
#   M13.2 POST /run/kvm/resume { run_id, node_id, branch_tapes_hex: [""], frame_run_ids } —
#         reconstruct that Universe (no kernel image, no reboot) and fork it with an empty suffix —
#         real boot+branch, frame_persistence[0].frames_recorded=1, this time via a restore-based row.
#   M13.3 GET  /runs/:id/frames — the real frame row landed in the DB.
#   M13.4 POST /runs/:id/stream/render (qoi-seq) — the restore-and-replay path decodes to the
#         guest's *actual* pixels (10,10,10),(20,20,20),(30,30,30),(40,40,40) — the same real pixels
#         drive/m11.sh's plain /run/kvm path and drive/m12.sh's branch path both prove, now reached
#         via a resume-originated run with no kernel reboot at all.
#   M13.5 Re-rendering the same run reproduces byte-identical output (restore-replay is deterministic).

set -euo pipefail

cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

REPO_ROOT="$(pwd)"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
SERVER_PID=""
DB_FILE="$(mktemp -t baud-m13-XXXXXX.sqlite)"
DB_FILE="$(cygpath -m "$DB_FILE" 2>/dev/null || echo "$DB_FILE")"
OUT_QOI="$(mktemp -t baud-m13-out-XXXXXX.qoi)"
OUT_QOI2="$(mktemp -t baud-m13-out2-XXXXXX.qoi)"

cleanup() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    sleep 0.2
    rm -f "$DB_FILE" "$OUT_QOI" "$OUT_QOI2" 2>/dev/null || true
}
trap cleanup EXIT

log() { echo "[m13] $*" >&2; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ $*" >&2; exit 1; }

FRAMEBUFFER_GUEST_KERNEL="$REPO_ROOT/crates/baud-multiverse/tests/fixtures/framebuffer-guest/bzImage"
[[ -f "$FRAMEBUFFER_GUEST_KERNEL" ]] || fail "fixture kernel missing: $FRAMEBUFFER_GUEST_KERNEL"

log "Building baud-server..."
cargo build -q --bin baud-server 2>&1

log "Starting baud-server (DB: $DB_FILE)..."
pkill -f "baud-server" 2>/dev/null || true; sleep 0.2
BAUD_DB="sqlite://${DB_FILE}?mode=rwc" BAUD_LOG=warn "$BAUD_SERVER_BIN" &
SERVER_PID=$!

for i in $(seq 1 30); do
    if curl -sf http://127.0.0.1:7734/health > /dev/null 2>&1; then
        break
    fi
    sleep 0.2
done
curl -sf http://127.0.0.1:7734/health > /dev/null || fail "baud-server did not start"
pass "baud-server is running (PID $SERVER_PID)"

SRV="http://127.0.0.1:7734"

# ---------------------------------------------------------------------------
# M13.1 — POST /run/kvm/branch { branch_tapes_hex: [], persist_run_id } — persist-only
# ---------------------------------------------------------------------------
log "--- M13.1: POST /run/kvm/branch { persist_run_id } — persist-only, no fork yet ---"
STORE_RUN_ID="m13-store-$$"
BRANCH=$(curl -sf -X POST "$SRV/run/kvm/branch" -H "Content-Type: application/json" \
    -d "{\"kernel_path\": \"$FRAMEBUFFER_GUEST_KERNEL\", \"branch_tapes_hex\": [], \"persist_run_id\": \"$STORE_RUN_ID\"}")
BRANCH_OK=$(echo "$BRANCH" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$BRANCH_OK" == "True" ]] || fail "M13.1: /run/kvm/branch returned ok!=true: $BRANCH"
RETURNED_RUN_ID=$(echo "$BRANCH" | python3 -c "import sys,json; print(json.load(sys.stdin)['persisted']['run_id'])")
NODE_ID=$(echo "$BRANCH" | python3 -c "import sys,json; print(json.load(sys.stdin)['persisted']['node_id'])")
[[ "$RETURNED_RUN_ID" == "$STORE_RUN_ID" ]] || fail "M13.1: persisted.run_id mismatch: $BRANCH"
[[ -n "$NODE_ID" ]] || fail "M13.1: persisted.node_id is empty: $BRANCH"
pass "M13.1: framebuffer-guest branch point persisted under run_id=$STORE_RUN_ID node_id=$NODE_ID"

# ---------------------------------------------------------------------------
# M13.2 — POST /run/kvm/resume { frame_run_ids } — restore, no reboot, persist a real Frame
# ---------------------------------------------------------------------------
log "--- M13.2: POST /run/kvm/resume { frame_run_ids } — restore-and-fork, real Frame drain ---"
RESUME_RUN_ID="m13-resume-frame-$$"
RESUME=$(curl -sf -X POST "$SRV/run/kvm/resume" -H "Content-Type: application/json" \
    -d "{\"run_id\": \"$STORE_RUN_ID\", \"node_id\": \"$NODE_ID\", \"branch_tapes_hex\": [\"\"], \"frame_run_ids\": [\"$RESUME_RUN_ID\"]}")
RESUME_OK=$(echo "$RESUME" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$RESUME_OK" == "True" ]] || fail "M13.2: /run/kvm/resume returned ok!=true: $RESUME"
FRAMES_RECORDED=$(echo "$RESUME" | python3 -c "import sys,json; print(json.load(sys.stdin)['frame_persistence'][0]['frames_recorded'])")
[[ "$FRAMES_RECORDED" == "1" ]] || fail "M13.2: expected frame_persistence[0].frames_recorded=1, got $FRAMES_RECORDED: $RESUME"
pass "M13.2: /run/kvm/resume restored+forked framebuffer-guest (no reboot) and persisted 1 real Frame record under run_id=$RESUME_RUN_ID"

# ---------------------------------------------------------------------------
# M13.3 — GET /runs/:id/frames — the real row landed in the DB
# ---------------------------------------------------------------------------
log "--- M13.3: GET /runs/:id/frames — real frame row in frame_records ---"
FRAMES=$(curl -sf "$SRV/runs/$RESUME_RUN_ID/frames")
FRAME_COUNT=$(echo "$FRAMES" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['frames']))")
[[ "$FRAME_COUNT" == "1" ]] || fail "M13.3: expected 1 frame row, got $FRAME_COUNT: $FRAMES"
FRAME_W=$(echo "$FRAMES" | python3 -c "import sys,json; print(json.load(sys.stdin)['frames'][0]['width'])")
FRAME_H=$(echo "$FRAMES" | python3 -c "import sys,json; print(json.load(sys.stdin)['frames'][0]['height'])")
FRAME_FMT=$(echo "$FRAMES" | python3 -c "import sys,json; print(json.load(sys.stdin)['frames'][0]['format'])")
[[ "$FRAME_W" == "2" && "$FRAME_H" == "2" && "$FRAME_FMT" == "indexed8" ]] \
    || fail "M13.3: unexpected frame geometry/format: w=$FRAME_W h=$FRAME_H fmt=$FRAME_FMT"
pass "M13.3: real frame row persisted (2x2 indexed8)"

# ---------------------------------------------------------------------------
# M13.4 — POST /runs/:id/stream/render — restore-and-replay, decodes to the real pixels
# ---------------------------------------------------------------------------
log "--- M13.4: POST /runs/:id/stream/render — restore-and-replay path (no kernel reboot) ---"
RENDER=$(curl -sf -X POST "$SRV/runs/$RESUME_RUN_ID/stream/render" -H "Content-Type: application/json" \
    -d "{\"format\": \"qoi-seq\", \"out\": \"$OUT_QOI\"}")
RENDER_OK=$(echo "$RENDER" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$RENDER_OK" == "True" ]] || fail "M13.4: render returned ok!=true: $RENDER"
RENDER_FRAMES=$(echo "$RENDER" | python3 -c "import sys,json; print(json.load(sys.stdin)['frame_count'])")
[[ "$RENDER_FRAMES" == "1" ]] || fail "M13.4: expected frame_count=1, got $RENDER_FRAMES: $RENDER"
[[ -s "$OUT_QOI" ]] || fail "M13.4: render did not write a non-empty output file"

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
[[ "$DECODED_OK" == "OK" ]] || fail "M13.4: decoded QOI pixels are not the guest's real pixels: $DECODED_OK"
pass "M13.4: rendered QOI decodes to the guest's real pixels (10,10,10),(20,20,20),(30,30,30),(40,40,40) — a resume-originated run replays for real via restore, not reboot, and not synthetically"

# ---------------------------------------------------------------------------
# M13.5 — re-rendering reproduces byte-identically (restore-replay is deterministic)
# ---------------------------------------------------------------------------
log "--- M13.5: re-render is byte-identical (restore-replay determinism) ---"
curl -sf -X POST "$SRV/runs/$RESUME_RUN_ID/stream/render" -H "Content-Type: application/json" \
    -d "{\"format\": \"qoi-seq\", \"out\": \"$OUT_QOI2\"}" > /dev/null
cmp -s "$OUT_QOI" "$OUT_QOI2" || fail "M13.5: re-render produced different bytes — restore-replay must be deterministic"
pass "M13.5: re-rendering the same run reproduces byte-identical output"

curl -sf http://127.0.0.1:7734/health > /dev/null || fail "baud-server is no longer healthy at the end of the run"
pass "baud-server remained healthy throughout"

echo ""
echo "==========================================="
echo "ALL M13 CHECKS PASSED"
echo "==========================================="
echo ""
echo "A resume-originated run also gets a real, replayable kvm_run_meta row, exercised end-to-end"
echo "against a real baud-server process on real /dev/kvm:"
echo "  POST /run/kvm/branch { persist_run_id }     — persist-only, establishes the point to resume from"
echo "  POST /run/kvm/resume { frame_run_ids }      — real restore+fork (no reboot) persists a real Frame record"
echo "  GET  /runs/:id/frames                       — real frame row in the DB"
echo "  POST /runs/:id/stream/render                — restore-and-replay decodes to the guest's actual pixels"
echo "  determinism                                 — re-rendering reproduces byte-identical output"
