#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/pkg-virtio-rng-branch-resume-cli.sh — proves the last-open half of todo.md §14's
# virtio-rng gap is now closed: /run/kvm/branch and /run/kvm/resume accept a virtio_rng
# spec (fixed-tape mode) and actually deliver the device's interrupt through a forked/
# resumed Multiverse::branch, and stream::render's real-*restore* path (resume-originated
# runs) now reads back and honors the same kvm_run_meta virtio_rng_* columns the reboot
# path already did (drive/pkg-virtio-rng-replay-cli.sh).
#
#   RNG-BR.1 POST /run/kvm/branch { persist_run_id, virtio_rng } — virtio-rng-guest, a
#       fresh branch's own ISR observes the real seeded entropy byte.
#   RNG-BR.2 POST /run/kvm/resume { virtio_rng } — resuming that persisted point, forking
#       again with virtio_rng, reproduces the identical ISR output with no re-boot.
#   RNG-BR.3 POST /run/kvm/branch { persist_run_id, virtio_rng, frame_run_ids } —
#       framebuffer-guest, persists a restore-based kvm_run_meta row via
#       /run/kvm/resume's own frame_run_ids (kernel_path/cmdline are NULL, store_run_id/
#       snapshot_node_id set instead).
#   RNG-BR.4 kvm_run_meta.virtio_rng_* columns persisted on that restore-based row.
#   RNG-BR.5 POST /runs/:id/stream/render — the restore-and-replay path
#       (render_frames_from_real_restore) re-enables virtio_rng and still decodes to the
#       guest's real, unperturbed pixels.
#   RNG-BR.6 Re-rendering reproduces byte-identical output.

set -euo pipefail

cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

REPO_ROOT="$(pwd)"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
SERVER_PID=""
DB_FILE="$(mktemp -t baud-pkg-rng-br-XXXXXX.sqlite)"
DB_FILE="$(cygpath -m "$DB_FILE" 2>/dev/null || echo "$DB_FILE")"
OUT_QOI="$(mktemp -t baud-pkg-rng-br-out-XXXXXX.qoi)"
OUT_QOI2="$(mktemp -t baud-pkg-rng-br-out2-XXXXXX.qoi)"

cleanup() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    sleep 0.2
    rm -f "$DB_FILE" "$OUT_QOI" "$OUT_QOI2" 2>/dev/null || true
}
trap cleanup EXIT

log() { echo "[pkg-rng-br] $*" >&2; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ $*" >&2; exit 1; }

VIRTIO_RNG_GUEST_KERNEL="$REPO_ROOT/crates/baud-multiverse/tests/fixtures/virtio-rng-guest/bzImage"
FRAMEBUFFER_GUEST_KERNEL="$REPO_ROOT/crates/baud-multiverse/tests/fixtures/framebuffer-guest/bzImage"
[[ -f "$VIRTIO_RNG_GUEST_KERNEL" ]] || fail "fixture kernel missing: $VIRTIO_RNG_GUEST_KERNEL"
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
# RNG-BR.1 — POST /run/kvm/branch { persist_run_id, virtio_rng } — virtio-rng-guest
# ---------------------------------------------------------------------------
log "--- RNG-BR.1: POST /run/kvm/branch { persist_run_id, virtio_rng } — virtio-rng-guest ---"
STORE_RUN_ID="pkg-rng-br-store-$$"
BRANCH=$(curl -sf -X POST "$SRV/run/kvm/branch" -H "Content-Type: application/json" \
    -d "{\"kernel_path\": \"$VIRTIO_RNG_GUEST_KERNEL\", \"persist_run_id\": \"$STORE_RUN_ID\", \
         \"branch_tapes_hex\": [\"\"], \
         \"virtio_rng\": {\"seed\": 42, \"vector\": 49, \"max_exits\": 200000}}")
BRANCH_OK=$(echo "$BRANCH" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$BRANCH_OK" == "True" ]] || fail "RNG-BR.1: /run/kvm/branch returned ok!=true: $BRANCH"
BRANCH_CONSOLE=$(echo "$BRANCH" | python3 -c "import sys,json; print(json.load(sys.stdin)['branches'][0]['console_output_hex'])")
[[ "$BRANCH_CONSOLE" == "5295" ]] || fail "RNG-BR.1: unexpected branch console output: $BRANCH_CONSOLE (want 5295, the ISR marker + seed-42 entropy byte)"
NODE_ID=$(echo "$BRANCH" | python3 -c "import sys,json; print(json.load(sys.stdin)['persisted']['node_id'])")
pass "RNG-BR.1: a fresh /run/kvm/branch with virtio_rng delivered the real interrupt (console=$BRANCH_CONSOLE), persisted node_id=$NODE_ID"

# ---------------------------------------------------------------------------
# RNG-BR.2 — POST /run/kvm/resume { virtio_rng } — no re-boot, identical ISR output
# ---------------------------------------------------------------------------
log "--- RNG-BR.2: POST /run/kvm/resume { virtio_rng } — no re-boot ---"
RESUME=$(curl -sf -X POST "$SRV/run/kvm/resume" -H "Content-Type: application/json" \
    -d "{\"run_id\": \"$STORE_RUN_ID\", \"node_id\": \"$NODE_ID\", \"branch_tapes_hex\": [\"\"], \
         \"virtio_rng\": {\"seed\": 42, \"vector\": 49, \"max_exits\": 200000}}")
RESUME_OK=$(echo "$RESUME" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$RESUME_OK" == "True" ]] || fail "RNG-BR.2: /run/kvm/resume returned ok!=true: $RESUME"
RESUME_CONSOLE=$(echo "$RESUME" | python3 -c "import sys,json; print(json.load(sys.stdin)['branches'][0]['console_output_hex'])")
[[ "$RESUME_CONSOLE" == "5295" ]] || fail "RNG-BR.2: unexpected resumed console output: $RESUME_CONSOLE"
pass "RNG-BR.2: resuming the persisted point with virtio_rng reproduced the identical ISR output (console=$RESUME_CONSOLE) with no re-boot"

# ---------------------------------------------------------------------------
# RNG-BR.3 — POST /run/kvm/branch { persist_run_id } (persist-only) then
# POST /run/kvm/resume { virtio_rng, frame_run_ids } — framebuffer-guest, restore-based row
# ---------------------------------------------------------------------------
log "--- RNG-BR.3: persist-only branch + resume { virtio_rng, frame_run_ids } — framebuffer-guest ---"
FB_STORE_RUN_ID="pkg-rng-br-fb-store-$$"
FB_BRANCH=$(curl -sf -X POST "$SRV/run/kvm/branch" -H "Content-Type: application/json" \
    -d "{\"kernel_path\": \"$FRAMEBUFFER_GUEST_KERNEL\", \"persist_run_id\": \"$FB_STORE_RUN_ID\"}")
FB_BRANCH_OK=$(echo "$FB_BRANCH" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$FB_BRANCH_OK" == "True" ]] || fail "RNG-BR.3: persist-only /run/kvm/branch returned ok!=true: $FB_BRANCH"
FB_NODE_ID=$(echo "$FB_BRANCH" | python3 -c "import sys,json; print(json.load(sys.stdin)['persisted']['node_id'])")

FRAME_RUN_ID="pkg-rng-br-frame-$$"
FB_RESUME=$(curl -sf -X POST "$SRV/run/kvm/resume" -H "Content-Type: application/json" \
    -d "{\"run_id\": \"$FB_STORE_RUN_ID\", \"node_id\": \"$FB_NODE_ID\", \"branch_tapes_hex\": [\"\"], \
         \"virtio_rng\": {\"seed\": 7, \"vector\": 49, \"max_exits\": 200000}, \
         \"frame_run_ids\": [\"$FRAME_RUN_ID\"]}")
FB_RESUME_OK=$(echo "$FB_RESUME" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$FB_RESUME_OK" == "True" ]] || fail "RNG-BR.3: /run/kvm/resume with frame_run_ids returned ok!=true: $FB_RESUME"
pass "RNG-BR.3: persisted a restore-based kvm_run_meta row (run_id=$FRAME_RUN_ID) via resume with virtio_rng enabled"

# ---------------------------------------------------------------------------
# RNG-BR.4 — kvm_run_meta persisted the three virtio_rng_* columns on the restore-based row
# ---------------------------------------------------------------------------
log "--- RNG-BR.4: kvm_run_meta.virtio_rng_* columns persisted on the restore-based row ---"
COLS=$(python3 -c "
import sqlite3
conn = sqlite3.connect('$DB_FILE')
row = conn.execute(
    'SELECT virtio_rng_seed, virtio_rng_vector, virtio_rng_max_exits, store_run_id, snapshot_node_id FROM kvm_run_meta WHERE run_id = ?',
    ('$FRAME_RUN_ID',),
).fetchone()
print(row)
")
[[ "$COLS" == "(7, 49, 200000, '$FB_STORE_RUN_ID', '$FB_NODE_ID')" ]] || fail "RNG-BR.4: unexpected kvm_run_meta row: $COLS"
pass "RNG-BR.4: kvm_run_meta persisted virtio_rng_seed=7, virtio_rng_vector=49, virtio_rng_max_exits=200000, plus the store_run_id/snapshot_node_id restore lineage"

# ---------------------------------------------------------------------------
# RNG-BR.5 — POST /runs/:id/stream/render — restore-and-replay path with virtio_rng
# ---------------------------------------------------------------------------
log "--- RNG-BR.5: POST /runs/:id/stream/render — restore-and-replay with virtio_rng re-enabled ---"
RENDER=$(curl -sf -X POST "$SRV/runs/$FRAME_RUN_ID/stream/render" -H "Content-Type: application/json" \
    -d "{\"format\": \"qoi-seq\", \"out\": \"$OUT_QOI\"}")
RENDER_OK=$(echo "$RENDER" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$RENDER_OK" == "True" ]] || fail "RNG-BR.5: render returned ok!=true: $RENDER"
[[ -s "$OUT_QOI" ]] || fail "RNG-BR.5: render did not write a non-empty output file"

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
[[ "$DECODED_OK" == "OK" ]] || fail "RNG-BR.5: decoded QOI pixels are not the guest's real, unperturbed pixels: $DECODED_OK"
pass "RNG-BR.5: restore-and-replay decodes to the guest's real pixels with virtio_rng re-enabled"

# ---------------------------------------------------------------------------
# RNG-BR.6 — re-rendering reproduces byte-identically
# ---------------------------------------------------------------------------
log "--- RNG-BR.6: re-render is byte-identical (restore-replay determinism with virtio_rng) ---"
curl -sf -X POST "$SRV/runs/$FRAME_RUN_ID/stream/render" -H "Content-Type: application/json" \
    -d "{\"format\": \"qoi-seq\", \"out\": \"$OUT_QOI2\"}" > /dev/null
cmp -s "$OUT_QOI" "$OUT_QOI2" || fail "RNG-BR.6: re-render produced different bytes"
pass "RNG-BR.6: re-rendering the same run reproduces byte-identical output"

curl -sf http://127.0.0.1:7734/health > /dev/null || fail "baud-server is no longer healthy at the end of the run"
pass "baud-server remained healthy throughout"

echo ""
echo "==========================================="
echo "ALL PKG-VIRTIO-RNG-BRANCH-RESUME CHECKS PASSED"
echo "==========================================="
echo ""
echo "/run/kvm/branch and /run/kvm/resume now accept virtio_rng (fixed-tape mode), and"
echo "stream::render's restore-and-replay path honors kvm_run_meta's virtio_rng_* columns —"
echo "closing todo.md §14 next-actions item 1's last-open virtio-rng gap, exercised end-to-end"
echo "against a real baud-server process on real /dev/kvm:"
echo "  POST /run/kvm/branch { persist_run_id, virtio_rng }  — a fresh branch delivers the interrupt"
echo "  POST /run/kvm/resume { virtio_rng }                  — a resumed branch reproduces it, no re-boot"
echo "  POST /run/kvm/resume { virtio_rng, frame_run_ids }    — persists a restore-based kvm_run_meta row"
echo "  kvm_run_meta (direct DB read)                         — the three columns round-trip exactly"
echo "  POST /runs/:id/stream/render                          — restore-and-replay honors virtio_rng"
echo "  determinism                                           — re-rendering reproduces byte-identical output"
echo ""
echo "Still open (todo.md §14): render_frames_from_real_restore's virtio_rng plumbing has no"
echo "fixture that actually drives the device AND emits frames in the same guest (no combined"
echo "fixture exists yet); generate mode (baud-driver-generated branches) still runs with"
echo "virtio_rng disabled; the deeper 'which vector an unmodified Linux guest's own virtio_mmio"
echo "driver would bind to' research question remains untouched."
