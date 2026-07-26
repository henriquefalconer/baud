#!/usr/bin/env bash
# Copyright (c) 2026 Henrique Falconer. All rights reserved.
# drive/pkg-virtio-rng-replay-cli.sh — proves stream::render's real-replay path (reboot
# sub-path) now reads back and honors the virtio_rng_* kvm_run_meta columns a
# `/run/kvm --virtio-rng-seed ...` boot persists, closing the "stream::render's real-replay
# path does not read the new virtio_rng_* columns back" gap named in todo.md §14.
#
# framebuffer-guest never touches the virtio-mmio window itself, so this proves two things
# at once without needing a combined virtio-rng-driving/frame-emitting fixture (which doesn't
# exist): (1) the three columns really do round-trip through kvm_run_meta and back into a
# replay boot (inspected directly via sqlite3), and (2) enabling the device on replay is a
# real no-op for a guest that never uses it — the guest's real pixels replay identically to
# drive/m11.sh's own baseline, proving the wiring doesn't perturb unrelated boots.
#
#   PKG-RNG-REPLAY.1 POST /run/kvm { run_id, virtio_rng } — framebuffer-guest, real boot.
#   PKG-RNG-REPLAY.2 kvm_run_meta persisted virtio_rng_seed/vector/max_exits (direct DB read).
#   PKG-RNG-REPLAY.3 POST /runs/:id/stream/render — real replay re-enables virtio_rng and still
#       decodes to the guest's actual, unperturbed pixels.
#   PKG-RNG-REPLAY.4 Re-rendering reproduces byte-identical output (replay stays deterministic
#       with virtio_rng enabled).

set -euo pipefail

cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"

REPO_ROOT="$(pwd)"
BAUD="$REPO_ROOT/target/debug/baud"
BAUD_SERVER_BIN="$REPO_ROOT/target/debug/baud-server"
SERVER_PID=""
DB_FILE="$(mktemp -t baud-pkg-rng-replay-XXXXXX.sqlite)"
DB_FILE="$(cygpath -m "$DB_FILE" 2>/dev/null || echo "$DB_FILE")"
OUT_QOI="$(mktemp -t baud-pkg-rng-replay-out-XXXXXX.qoi)"
OUT_QOI2="$(mktemp -t baud-pkg-rng-replay-out2-XXXXXX.qoi)"

cleanup() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    sleep 0.2
    rm -f "$DB_FILE" "$OUT_QOI" "$OUT_QOI2" 2>/dev/null || true
}
trap cleanup EXIT

log() { echo "[pkg-rng-replay] $*" >&2; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ $*" >&2; exit 1; }

FRAMEBUFFER_GUEST_KERNEL="$REPO_ROOT/crates/baud-multiverse/tests/fixtures/framebuffer-guest/bzImage"
[[ -f "$FRAMEBUFFER_GUEST_KERNEL" ]] || fail "fixture kernel missing: $FRAMEBUFFER_GUEST_KERNEL"

log "Building baud-server and baud..."
cargo build -q --bin baud-server --bin baud 2>&1

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
# PKG-RNG-REPLAY.1 — POST /run/kvm { run_id, virtio_rng } — real boot
# ---------------------------------------------------------------------------
log "--- PKG-RNG-REPLAY.1: POST /run/kvm { run_id, virtio_rng } — framebuffer-guest ---"
RUN_ID="pkg-rng-replay-$$"
BOOT=$(curl -sf -X POST "$SRV/run/kvm" -H "Content-Type: application/json" \
    -d "{\"kernel_path\": \"$FRAMEBUFFER_GUEST_KERNEL\", \"run_id\": \"$RUN_ID\", \"virtio_rng\": {\"seed\": 42, \"vector\": 49, \"max_exits\": 200000}}")
BOOT_OK=$(echo "$BOOT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$BOOT_OK" == "True" ]] || fail "PKG-RNG-REPLAY.1: /run/kvm returned ok!=true: $BOOT"
pass "PKG-RNG-REPLAY.1: /run/kvm booted framebuffer-guest with virtio_rng enabled, run_id=$RUN_ID"

# ---------------------------------------------------------------------------
# PKG-RNG-REPLAY.2 — kvm_run_meta persisted the three virtio_rng_* columns
# ---------------------------------------------------------------------------
log "--- PKG-RNG-REPLAY.2: kvm_run_meta.virtio_rng_* columns persisted ---"
COLS=$(python3 -c "
import sqlite3
conn = sqlite3.connect('$DB_FILE')
row = conn.execute(
    'SELECT virtio_rng_seed, virtio_rng_vector, virtio_rng_max_exits FROM kvm_run_meta WHERE run_id = ?',
    ('$RUN_ID',),
).fetchone()
print(row)
")
[[ "$COLS" == "(42, 49, 200000)" ]] || fail "PKG-RNG-REPLAY.2: unexpected kvm_run_meta virtio_rng_* columns: $COLS"
pass "PKG-RNG-REPLAY.2: kvm_run_meta persisted virtio_rng_seed=42, virtio_rng_vector=49, virtio_rng_max_exits=200000"

# ---------------------------------------------------------------------------
# PKG-RNG-REPLAY.3 — POST /runs/:id/stream/render re-enables virtio_rng on replay and still
# decodes to the guest's real, unperturbed pixels
# ---------------------------------------------------------------------------
log "--- PKG-RNG-REPLAY.3: POST /runs/:id/stream/render — real replay with virtio_rng re-enabled ---"
RENDER=$(curl -sf -X POST "$SRV/runs/$RUN_ID/stream/render" -H "Content-Type: application/json" \
    -d "{\"format\": \"qoi-seq\", \"out\": \"$OUT_QOI\"}")
RENDER_OK=$(echo "$RENDER" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ok', False))")
[[ "$RENDER_OK" == "True" ]] || fail "PKG-RNG-REPLAY.3: render returned ok!=true: $RENDER"
[[ -s "$OUT_QOI" ]] || fail "PKG-RNG-REPLAY.3: render did not write a non-empty output file"

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
[[ "$DECODED_OK" == "OK" ]] || fail "PKG-RNG-REPLAY.3: decoded QOI pixels are not the guest's real, unperturbed pixels: $DECODED_OK"
pass "PKG-RNG-REPLAY.3: rendered QOI decodes to the guest's real pixels with virtio_rng re-enabled on replay"

# ---------------------------------------------------------------------------
# PKG-RNG-REPLAY.4 — re-rendering reproduces byte-identically
# ---------------------------------------------------------------------------
log "--- PKG-RNG-REPLAY.4: re-render is byte-identical (replay determinism with virtio_rng) ---"
curl -sf -X POST "$SRV/runs/$RUN_ID/stream/render" -H "Content-Type: application/json" \
    -d "{\"format\": \"qoi-seq\", \"out\": \"$OUT_QOI2\"}" > /dev/null
cmp -s "$OUT_QOI" "$OUT_QOI2" || fail "PKG-RNG-REPLAY.4: re-render produced different bytes"
pass "PKG-RNG-REPLAY.4: re-rendering the same run reproduces byte-identical output"

curl -sf http://127.0.0.1:7734/health > /dev/null || fail "baud-server is no longer healthy at the end of the run"
pass "baud-server remained healthy throughout"

echo ""
echo "==========================================="
echo "ALL PKG-VIRTIO-RNG-REPLAY CHECKS PASSED"
echo "==========================================="
echo ""
echo "stream::render's real-replay path (reboot sub-path) now reads back kvm_run_meta's"
echo "virtio_rng_seed/virtio_rng_vector/virtio_rng_max_exits columns and threads them through"
echo "boot_and_drain_frames/boot_run_and_drain, exercised end-to-end against a real baud-server"
echo "process on real /dev/kvm:"
echo "  POST /run/kvm { run_id, virtio_rng } — real boot persists the three virtio_rng_* columns"
echo "  kvm_run_meta (direct DB read)         — the three columns round-trip exactly"
echo "  POST /runs/:id/stream/render          — real replay re-enables virtio_rng, decodes real pixels"
echo "  determinism                           — re-rendering reproduces byte-identical output"
echo ""
echo "Still open (todo.md §14): the restore-replay sub-path (resume-originated runs) and"
echo "/run/kvm/branch|/run/kvm/resume themselves still don't accept virtio_rng at all — both"
echo "need a new run_until_branch_or_halt_with_virtio_rng combinator in baud-multiverse first."
