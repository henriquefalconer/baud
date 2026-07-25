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

## Building an out-of-tree kernel module against this WSL2 kernel

The stock WSL2 kernel ships no `linux-headers-*` package, so `/lib/modules/$(uname -r)/build` is
missing by default. To build one (needed for `kernel-module/baud-enforced/`, and any future
out-of-tree KVM module work):

```
mkdir -p ~/wsl-kernel-src && cd ~/wsl-kernel-src
git clone --depth 1 --branch linux-msft-wsl-$(uname -r | sed 's/-microsoft-standard-WSL2//') \
    https://github.com/microsoft/WSL2-Linux-Kernel.git src
cd src && rm -rf .git   # shallow clone defeats scripts/setlocalversion's tag lookup, which then
                        # appends a spurious "+" to kernelrelease and breaks vermagic matching
zcat /proc/config.gz > .config
sudo apt-get install -y gcc-13   # match the running kernel's actual build-gcc major version;
                                  # the default gcc here is newer and changes struct ABI details
                                  # (e.g. CONFIG_CC_HAS_COUNTED_BY) that stock gcc doesn't have
make CC=gcc-13 olddefconfig && make CC=gcc-13 modules_prepare -j$(nproc)
sudo ln -sfn "$PWD" "/lib/modules/$(uname -r)/build"
```

Build modules with `KBUILD_MODPOST_WARN=1 make CC=gcc-13` (a headers-only tree has no
`Module.symvers`, so modpost can't resolve ordinary exported symbols like `printk` at build
time — this is expected, not a real error; resolution happens correctly at `insmod` time).
As of this writing `insmod` still fails with a struct-module-size ABI mismatch even with a
matching-major-version gcc, because Microsoft's exact build toolchain (gcc 13.2.0 + binutils
2.41) differs from any Ubuntu-packaged substitute — see `kernel-module/baud-enforced/BUILD.md`
for the full diagnosis.

## Git push from WSL2

WSL2's native Linux `git` has no credential helper configured, so a plain `git push` fails with
"could not read Username". The Windows side's `gh.exe` (on `PATH` via WSL interop) is already
authenticated, so bridge to it once per clone:
```
git config credential.helper "!gh.exe auth git-credential"
```
then `git push` works normally.

## Building / testing

```
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

Drive scripts (`drive/h0.sh`, `drive/h1.sh`, `drive/m0.sh`, …) build only what they need and run the CLI
against a locally-spawned `baud-server` on a temp SQLite file — see any `drive/*.sh` for the pattern
(spawn server, `trap cleanup EXIT`, run `baud <cmd> --json`, assert on the JSON).
