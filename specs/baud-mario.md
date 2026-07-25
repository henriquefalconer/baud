<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Mario Example Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-25

---

## 1. Overview

### Purpose

`examples/mario` is a validation target, not infrastructure: an unmodified NES emulator (FCEUX) running
inside a real Linux guest, driven only by the tape, that baud's fuzzer explores until it **completes Super
Mario Bros**. It is the visible instance of the generic loop (`todo.md` §0, §11): any program on baud's
deterministic Linux is a system baud drives to a chosen state, reproducibly. It exercises the same machine,
image pipeline, harness contract, driver, stream, and CLI as any other workload — the only Mario-specific
code is this example.

### Goals

- **A deep goal reachable only by exploration**: completion needs "sequential luck" over hundreds of frames.
- **Emulator runs inside Linux**: FCEUX headless in the guest image, driven frame-by-frame from the tape.
- **Zero special treatment**: deployed via `examples/mario/spec.toml` like any workload; no game knowledge in
  any crate under `crates/` (`no_workload_specifics_in_core`).
- **Watchable**: every run streams the emulator live (~25% of the screen); the winning run produces the
  centralized README GIF.

### Non-Goals

- Being a general NES test suite; SMB is the demonstration.
- Bundling any ROM (copyright) — the ROM + optional savestate are user-supplied paths.
- Any new engine capability — exploration is `baud-driver`'s, reused unchanged.

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────────────┐
│   examples/mario/  (a TARGET, not a crate)             │
│  spec.toml     → a real Linux image (baud-packages)    │
│                   kernel + initramfs + FCEUX + Lua      │
│  harness.lua   → tape byte ↔ joypad, frame step,        │
│                   RAM probes out, frame out (opcode)    │
│  probes.toml   → x / world / area / oper_mode + goal    │
│  strategy.toml → maximize x; sticky-mask + grid         │
└──────────────────────────────────────────────────────┘
   ▲ boots on the machine like any H7 guest · deployed via spec.toml
```

### Rationale

- No new crate and no vendored NES core — the SUT is upstream FCEUX, unmodified. The only code shipped is the
  Lua harness (the `baud-guest-harness` contract) plus TOML config. Everything else is generic baud.

---

## 3. The Program & Goal

- **Guest image** (`spec.toml`, built by `baud-packages` §4.5): a real Linux image with FCEUX, a Lua
  interpreter, `harness.lua`, and a static `/init` that launches the emulator headless
  (`SDL_VIDEODRIVER=dummy SDL_AUDIODRIVER=dummy fceux --no-config 1 --sound 0 --loadlua /harness.lua
  /game.nes`) and powers off on exit.
- **Determinism seams pinned**: `RAMInitOption ∈ {0,1,2}` (fixed power-on RAM), start from power-on or a fixed
  savestate — so FCEUX emulation is a pure function of `(ROM, input tape)`, and the whole guest is a pure
  function of the tape.
- **Goal**: complete the game. SMB has no single "game-completed" byte — the end state is a *derived*
  predicate (world 8 cleared through the final castle), evaluated over the §4 probes.

---

## 4. Probes (`examples/mario/probes.toml`)

Read from NES RAM by `harness.lua` each frame (confirmed against the SMB RAM map + the 6502 disassembly):

| Probe | Address(es) | Meaning |
|-------|-------------|---------|
| `x` | `mem[0x006D]*256 + mem[0x0086]` | global horizontal position (page × 256 + on-screen x) — the progress signal |
| `y` | `mem[0x00CE]` | on-screen vertical position — the grid's second dimension |
| `world` | `mem[0x075F]` | world number (0-based) |
| `area` | `mem[0x0760]` | level / area number |
| `lives` | `mem[0x075A]` | remaining lives |
| `oper_mode` | `mem[0x0770]` | game mode: `01` play, `02` end-of-world, `03` end / dead |

The **completion predicate** (`goal` in `probes.toml`): `world == 7` cleared through the final castle,
detected via the `oper_mode` / `area` transition. `baud image lint` validates every address against a
reference RAM map — never hard-coded on faith.

---

## 5. Strategy & Tactics (for the drive)

| Aspect | Value |
| ------ | ----- |
| strategy | `maximize = ["probe:x"]`, `buckets = ["probe:x","probe:y"]` — go right; the `(x,y)` novelty grid escapes dead ends and finds the warp route |
| tactics | `sticky-mask` (correlated input: `next = prev XOR low-p mask`, so buttons stay held across the 30–100 frames a jump needs) — finds it; `random` (fresh byte per frame, positions plateau near spawn) — negative control |

Both are generic `baud-driver` primitives selected here; the example adds none of its own exploration.

---

## 6. Live view & the README GIF

- Every run streams the emulator framebuffer via the `FRAME` opcode (`baud-guest-harness` §6 →
  `baud-stream`), rendered to a ~25%-of-screen live window (`baud stream tail --run <id> --format y4m |
  ffplay …`, `todo.md` §11.7).
- Frames are a pure function of the tape, so only the tape is stored; the winning run's centralized README
  GIF is regenerated from its tape (`drive/mario-gif.sh`: `baud stream tail … | ffmpeg … docs/mario.gif`) —
  a reproducible artifact, not a screen recording.

---

## 7. Testing (H8 drive, `drive/mario.sh`)

```rust
#[test] fn interactive_probe_stream_is_identical() {
    let (a, b) = (run(mario(), tape.clone()), run(mario(), tape.clone()));
    assert_eq!(a.probe_stream(), b.probe_stream());        // same tape ⇒ identical probes
    assert_eq!(a.frame_hashes(), b.frame_hashes());        // …and identical frames
}

#[test] fn completion_is_reachable_and_shrinks() {
    let win = run(mario(), guided()).expect_goal();        // GoalReached on the completion predicate
    let small = shrink(win.tape);
    assert!(run_tape(mario(), &small).reached_goal());      // the shrunk tape still completes
}
```

The `drive/mario.sh` script wraps the same sequence: `image build` + `image lint` → `verify determinism`
(identical probe + frame hashes) → negative control (`--tactics random` plateaus) → guided run reaching
completion → mid-run `tape kill` + `reconstruct` + resume → `shrink` → `replay` still completes → the
non-fragility case on a harder ROM variant (non-gating).

---

## 8. Considerations

| Concern | Handling |
|---------|----------|
| ROM licensing | ROM + savestate are user-supplied paths, never bundled; CI uses a free homebrew ROM |
| Emulator footprint | One FCEUX process in a single-vCPU guest; the whole image is content-addressed |
| Special requirements | None; same machine / image / harness / driver / CLI as every workload |
| Completion signal | Derived from world / `oper_mode`, not a single flag (§4) |

---

## 9. Security Considerations

| Threat | Handling |
|--------|----------|
| Target treated as trusted | It is a guest under the machine; same mediation as any workload |
| Game code in core | Forbidden — `no_workload_specifics_in_core` fails the build if any crate references it |

---

## 10. Future Considerations

| Feature | Description |
|---------|-------------|
| Other emulated targets | The same harness shape drives other deterministic emulators / games |
| Finer completion oracle | Pin the exact end-of-8-4 predicate once verified, replacing the world-progress CI gate |
| Speed-focused strategy | A frame-count-minimizing objective for a fastest-completion demonstration |
