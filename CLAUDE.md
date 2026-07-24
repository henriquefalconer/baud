<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# CLAUDE.md — operational notes

Status/progress/history live in `todo.md` and `ralph/progress.txt`, not here. This file is only
"how do I actually run the thing on this machine."

## Environment

The dev/build environment is **Ubuntu on WSL2**, running on a bare-metal Dell XPS 13 9310 (Intel, VT-x
enabled), so **`/dev/kvm` is available natively** and the whole stack — including the KVM VMM
(`baud-multiverse`) and all `cfg(target_os = "linux")` code — builds, links, and runs here directly, with
no cross-target or check-only workarounds.

The login is **username `baud` / password `baud`**; use the password for `sudo` non-interactively, e.g.
`echo baud | sudo -S <cmd>`.

## Toolchain

Native Linux toolchain (one-time, already installed):

```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
sudo apt-get update && sudo apt-get install -y build-essential python3 pkg-config
```

`rustc`/`cargo` default to `x86_64-unknown-linux-gnu`, so `cargo build` compiles and links the real KVM
code (`kvm-ioctls`, `perf-event`, `userfaultfd`, …) directly.

## KVM host

`/dev/kvm` is present on this machine. Confirm it and grant access once:

```
ls -l /dev/kvm && grep -c vmx /proc/cpuinfo      # device exists; VT-x count > 0
sudo usermod -aG kvm "$USER"                      # open /dev/kvm without sudo (re-login after)
cargo run -p baud-cli -- host probe --json        # regime must NOT be "rejected"
```

H1+ (booting a real guest) runs here directly, e.g. `bash drive/h1.sh`. If `/dev/kvm` is ever missing,
VT-x is off in firmware — everything else is already in place.

## Building / testing

```
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

Drive scripts (`drive/h0.sh`, `drive/h1.sh`, `drive/m0.sh`, …) build only what they need and run the CLI
against a locally-spawned `baud-server` on a temp SQLite file — see any `drive/*.sh` for the pattern
(spawn server, `trap cleanup EXIT`, run `baud <cmd> --json`, assert on the JSON).
