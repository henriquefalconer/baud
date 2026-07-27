# Mario under baud-multiverse

Super Mario Bros (NES) running as a deterministic guest under baud-multiverse.

## What this is

The NES emulator core (`nes-bridge`) is a single-threaded, statically linked
C program satisfying the baud guest contract. The supervisor enforces determinism
on it exactly like any other guest: all syscalls mediated, all entropy from the
tape, virtual clock, no-PIE, musl.

Controller bytes arrive via a `fifo` input adapter. The bridge fixture reads NES
RAM at each frame boundary and emits memory probes via stdout-kv:

| Probe         | NES RAM address | Meaning                        |
|---------------|-----------------|--------------------------------|
| `x_page`      | `0x006D`        | Current screen page            |
| `x`           | `0x0086`        | Mario X position on screen     |
| `x_global`    | computed        | Global X = x_page * 256 + x   |
| `y`           | `0x00CE`        | Mario Y position               |
| `y_band`      | computed        | y / 30 (0-7 rough vertical)    |
| `world`       | `0x075F`        | World number (0-7)             |
| `level`       | `0x075C`        | Level within world (0-3)       |
| `lives`       | `0x075A`        | Lives remaining                |
| `game_over`   | flag            | 1 when game-over screen active |
| `game_completed` | flag         | 1 when Mario completes 8-4     |

The NES framebuffer (256x240 indexed8) is written to the `frame` fifo adapter
at each virtual frame step and journaled as frame hashes. Pixel bytes are NOT
stored during fuzz runs — only hashes. Use `baud stream render` to materialize
frames from a tape after the fact.

## ROM

**The NES ROM is NOT included and must be user-supplied.**

baud never bundles, downloads, or distributes copyrighted ROMs. The ROM path
is supplied at run time:

```
baud run start \
    --spec examples/mario/spec.yaml \
    --spec-param rom_path=/path/to/mario.nes \
    --strategy examples/mario/strategy.toml \
    --tactics stateful-mask
```

CI uses a homebrew ROM (public domain replacement). See `drive/m/m8.sh` for the
CI variant which accepts `world >= 2` as the success threshold.

## Strategy

Lexicographic maximization: `world` > `level` > `x_global`. Grid exploration
over `x_page` × `y_band` ensures the corpus covers different positions in the
same world, not just the furthest single path.

`stateful-mask{p_flip=0.03}` flips individual controller bits with 3%
probability per bit per byte. This models "sticky" button presses (Mario runs
further if you hold right) and is the dominant tactic for progress.

The `random` tactic is included as a negative control: without directed input
generation, random tactics plateau immediately (Mario never passes the first
gap without holding right).

## Quickstart

```bash
# Negative control — random tactics plateau (world=1, level=1, no progress)
baud run start --spec examples/mario/spec.yaml \
    --spec-param rom_path=/path/to/mario.nes \
    --tactics random --seed 1 --budget-minutes 5

# Main run — stateful-mask climbs worlds/levels
baud run start --spec examples/mario/spec.yaml \
    --spec-param rom_path=/path/to/mario.nes \
    --strategy examples/mario/strategy.toml \
    --tactics stateful-mask --seed 42 --budget-minutes 600

# Watch live progress
baud obs tail --run <run-id> --probe world
baud stream tail --run <run-id> -o live.y4m

# Render the completion video from the winning tape alone
baud stream render --run <run-id> --format y4m -o mario-completion.y4m
```

## Architecture notes

- **Agent binary**: the one built at M2, unmodified. Zero Mario-specific code
  in any baud crate. The Mario spec runs on the same adapters (fifo, stdout-kv,
  exit-hash, frame) as framedemo.
- **Supervisor**: enforces determinism the same way for the NES emulator core as
  for the hello-world or raftlet guests. The emulator receives no special trust.
- **Determinism check**: `baud verify determinism --spec examples/mario/spec.yaml`
  must pass before any fuzz run is considered valid.
- **Reconstruction**: `baud tape reconstruct` replays the tape prefix under a
  fresh supervisor — O(steps) cost; no mid-run snapshots.
