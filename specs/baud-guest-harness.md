<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Guest Harness Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-25

---

## 1. Overview

### Purpose

`baud-guest-harness` is the contract for the tiny in-guest **harness** — the only workload-aware code — that
bridges a software-under-test (SUT) to the one tape device so that `(image, tape)` fully determines execution
and observations. It defines how the harness reads input records from the tape, drives the SUT one step at a
time, and emits observations (probes, `goal`/`violation` markers, frames) back on the same channel. It is a
*contract*, realized per workload as a small program in the guest image (`examples/<name>/harness.*`) — not a
crate under `crates/`.

### Goals

- **One generic contract** every workload implements; the machine, driver, and CLI stay workload-agnostic.
- **Single channel**: all SUT input arrives as tape bytes; all observations return through the same tape
  device (`specs/baud-tape-device.md`).
- **Deterministic by construction**: the harness performs no wall-clock read, host file access, or entropy
  draw — only the tape endpoint.

### Non-Goals

- Workload semantics (the example supplies the byte↔stimulus mapping and the probe meanings).
- The device model (`baud-tape-device`), the boot pipeline (`baud-boot`), or frame rendering (`baud-stream`).

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────────────────┐
│  guest image (initramfs)                                   │
│   ┌─────────────┐   step: read → drive → observe           │
│   │   harness    │ ⇄ /dev/tape (the one tape device)        │
│   └──────┬──────┘        ▲ input bytes   ▼ outbound + opcode │
│          ▼ drives                                            │
│   ┌─────────────┐                                            │
│   │     SUT      │  (unmodified: emulator, server, parser…)  │
│   └─────────────┘                                            │
└──────────────────────────────────────────────────────────┘
        the harness is the ONLY workload-aware code · examples/<name>/
```

### Rationale

- Not a crate — a contract realized per example. The endpoint is the guest-side view of the one paravirtual
  tape device (`baud-boot` §exposes it as `/dev/tape`, a char shim over the device's PIO/MMIO ports; a
  virtio-serial port is an equivalent transport). Deps of an example harness: the tape endpoint + the SUT.
- Swapping workloads = a new `examples/<name>/` (harness + probes + strategy); the core never changes.

---

## 3. The endpoint (`/dev/tape`)

The harness talks to the one tape device through a single guest endpoint. `specs/baud-tape-device.md` §3–§4
is the **single source of truth** for the register layout and control opcodes — not restated here. The guest
view:

| Operation | Mechanism |
|-----------|-----------|
| Read one input byte | `read()` on `/dev/tape` — pops the next tape byte; blocks until available; EOF ⇒ tape closed |
| Append an outbound byte | `write()` on `/dev/tape` — appends to the current outbound record |
| Finalize a record | a control write with an opcode: `PROBE` / `MARK_BRANCH` / `GOAL` / `VIOLATION` / `LOG` / `FRAME` |

A userspace harness (e.g. Lua) uses ordinary `io.open("/dev/tape", "r+b")` + `read`/`write`; the control
opcode is issued via the endpoint's control write (an `ioctl`, a reserved sentinel, or a second
virtio-serial control port — the transport's choice, per `baud-boot`).

---

## 4. Input protocol (tape → SUT)

One record per step; the harness translates opaque bytes into SUT stimuli. `read` blocking is what paces the
SUT to the tape — exactly one step advances per input record.

```lua
local dev = assert(io.open("/dev/tape", "r+b"))

local raw = dev:read(1)             -- one step's input record; nil ⇒ tape closed
if not raw then dev:write_opcode("GOAL"); os.exit(0) end
apply_to_sut(string.byte(raw))      -- workload-specific: byte → SUT stimulus
```

Multi-byte records are read as a fixed-width or length-prefixed field the workload defines; the tape device
guarantees the byte order is a fixed function of the tape (`baud-tape-device` §5).

---

## 5. Observation protocol (SUT → tape)

Observations are opaque records the driver interprets (`specs/baud-driver.md`). Probes are `key=value`; `GOAL`
/ `VIOLATION` are control markers that set the run outcome (exit `2`).

```lua
step_sut()                                  -- advance the SUT exactly one step

dev:write("x="); dev:write(tostring(read_state())); dev:write_opcode("PROBE")
if goal_predicate() then dev:write_opcode("GOAL") end          -- → GoalReached
if invariant_broken() then dev:write("inv"); dev:write_opcode("VIOLATION") end
```

- A `PROBE` becomes `baud_proto::Msg::Observe{ probe, value }` at the current step; the driver scores /
  buckets on it.
- `GOAL` → `GoalReached`; `VIOLATION` → `Crash{ invariant }`. No temporal operators — everything is
  crash / invariant / goal, as in `todo.md` §7.

---

## 6. Frame protocol (framebuffer, optional)

A graphical SUT emits one frame per step for `baud-stream`. The byte layout — a one-byte pixel-format tag,
`u32` width, `u32` height, the raw pixel buffer, then the `FRAME` control opcode — is defined once in
`specs/baud-tape-device.md` §4 and consumed by `specs/baud-stream.md` §3. The harness only assembles it:

```lua
local px = capture_rgb24()                   -- w*h*3 bytes from the SUT
dev:write(string.char(FORMAT_RGB24))
dev:write(u32le(w)); dev:write(u32le(h)); dev:write(px)
dev:write_opcode("FRAME")
```

Frames are a pure function of the tape, so only the tape is journaled; `baud-stream` re-derives identical
frames by replay (`baud-stream` §5). No pixels are stored during a fuzz run.

---

## 7. Determinism Properties

- The harness performs no wall-clock read, host-file access, or entropy draw — only the tape endpoint.
- `read` blocking paces the SUT to the tape (one step per input record), so timing is a function of the tape.
- Given the same image + tape, the probe / observation / frame streams are byte-identical (a double-run
  under `baud verify determinism`).
- All workload knowledge lives in `examples/<name>/`; no crate under `crates/` references it.

---

## 8. Testing

```rust
#[test] fn harness_output_is_tape_derived() {
    let a = run(example("mario"), tape.clone()).observations();
    assert_eq!(a, run(example("mario"), tape.clone()).observations());   // same tape ⇒ same
    assert_ne!(a, run(example("mario"), flip_one_byte(&tape)).observations());
}

#[test] fn no_workload_specifics_in_core() {
    // no crate under crates/ references example probe symbols / emulator names
    assert!(scan_crates(&["fceux", "nes", "0x0086", "joypad"]).is_empty());
}

#[test] fn goal_and_violation_set_the_outcome() {
    assert!(matches!(run(goal_harness(), tape()).outcome, Some(GoalReached { .. })));
    assert!(matches!(run(violation_harness(), tape()).outcome, Some(Crash { .. })));
}
```

---

## 9. Security Considerations

| Threat | Handling |
|--------|----------|
| Harness reaches host state | Only the tape endpoint exists inside the guest — no host FS / clock / net |
| Probe values carry secrets | Opaque bytes; the store encrypts at rest (`baud-snapshot-store`) |
| Workload code leaks into core | `no_workload_specifics_in_core` fails the build if it does |

---

## 10. Future Considerations

| Feature | Description |
|---------|-------------|
| Harness SDK | A tiny C / Rust helper implementing §3–§6 so non-Lua workloads adopt the contract quickly |
| Multi-record steps | A length-prefixed input framing for SUTs that consume more than one byte per step |
| Bidirectional simulation | Harness-side simulation of network / storage responses fed from the tape |
