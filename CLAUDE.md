<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# CLAUDE.md — operational notes

Status/progress/history live in `todo.md` and `ralph/progress.txt`, not here. This file is only
"how do I actually run the thing on this machine."

## Toolchain (Windows dev machine)

This machine ships with **no Rust toolchain and no C linker** out of the box. One-time setup
(already done as of 2026-07-24, but if a fresh machine needs it again):

1. Install rustup: download `https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe`
   and run `rustup-init.exe -y --default-toolchain stable --profile default`.
2. The `msvc` Rust target needs Visual Studio Build Tools (`cl.exe`/`link.exe`), which requires an
   **interactive UAC elevation prompt** — this fails non-interactively (`vs_buildtools.exe` exits
   `1602`, "User may have declined UAC prompt"). Do not rely on the msvc toolchain here.
3. Instead, use the **GNU** toolchain, which needs only a portable MinGW-w64 GCC (no installer, no
   admin rights): download a `winlibs-x86_64-*-mingw-w64ucrt-*.zip` release from
   `https://github.com/brechtsanders/winlibs_mingw/releases`, extract it anywhere (e.g.
   `%USERPROFILE%\mingw64-tools`), then:
   ```
   rustup toolchain install stable-x86_64-pc-windows-gnu
   rustup default stable-x86_64-pc-windows-gnu
   ```
4. `rustup target add x86_64-unknown-linux-gnu` — lets `cargo check`/`cargo clippy --target
   x86_64-unknown-linux-gnu` type-check Linux-only code (e.g. `crates/baud-host/src/linux.rs`'s
   `kvm-ioctls`/`perf-event` usage) against the real crate sources **without needing a Linux
   host or linker** (`check`/`clippy` don't link). This is how Linux-only KVM code gets validated
   from this Windows box.
5. Both cargo's bin dir and the mingw bin dir must be on `PATH`:
   `%USERPROFILE%\.cargo\bin` and `%USERPROFILE%\mingw64-tools\mingw64\bin`. These were persisted
   to the **user** PATH (`[Environment]::SetEnvironmentVariable("Path", ..., "User")`), so new
   sessions/processes get them automatically — but an already-running shell (this session's Bash
   tool) does not re-read the registry, so every command in such a shell must prefix:
   ```
   export PATH="$HOME/.cargo/bin:$HOME/mingw64-tools/mingw64/bin:$PATH"
   ```
   Drive scripts under `drive/` already do this at the top.

## Running Rust inside WSL2 (from the Git Bash tool)

To run cargo/rustc inside WSL2 Ubuntu from Git Bash, invoke them by absolute Linux path with MSYS path-conversion disabled — e.g. `MSYS_NO_PATHCONV=1 wsl.exe -d Ubuntu -u baud -- /home/baud/.cargo/bin/cargo build` — because WSL inherits Git Bash's `HOME`/`PATH`, which otherwise shadows the Linux rustup with the Windows one and leaves cargo/rustc unresolved on `PATH`.

## No real KVM host here

This dev machine has no `/dev/kvm` (Windows, and WSL2 has no distro installed — `wsl --status`
shows the feature enabled but `wsl -l -v` lists nothing). `baud host probe` correctly reports
`regime: rejected` here; this is expected, not a bug. See `docs/determinism.md`'s H0 section.
Real KVM-hardware validation of `crates/baud-host/src/linux.rs`, and all of H1+ (booting a real
guest), needs an actual Linux/KVM host — bare-metal or a WSL2 distro with nested virt enabled.

**Bare-metal check + WSL2 setup (one-liner):** if `Get-CimInstance Win32_ComputerSystem` shows a real
vendor/model (e.g. `Dell Inc. / XPS 13 9310`, not "Virtual Machine" — a "hypervisor detected" line is just
VBS/Hyper-V on real hardware, not a guest), it *is* a valid KVM host, so enable Intel VT-x in BIOS, then from
an **elevated PowerShell** run `wsl --install -d Ubuntu`, set `nestedVirtualization=true` under `[wsl2]` in
`%UserProfile%\.wslconfig`, `wsl --shutdown`, and confirm `ls /dev/kvm` inside WSL2.

## Building / testing

```
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

Drive scripts (`drive/h0.sh`, `drive/m0.sh`, …) build only what they need and run the CLI against
a locally-spawned `baud-server` on a temp SQLite file — see any `drive/*.sh` for the pattern
(spawn server, `trap cleanup EXIT`, run `baud <cmd> --json`, assert on the JSON).
