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
sudo apt-get install -y dwarves   # pahole — MUST be installed before olddefconfig below, or
                                  # CONFIG_DEBUG_INFO_BTF_MODULES silently drops out of
                                  # .config and insmod later fails on a struct-module-size
                                  # mismatch (24 bytes / 4 fields short) — see
                                  # kernel-module/baud-enforced/BUILD.md for the full diagnosis
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
With `dwarves` installed before `olddefconfig`, `insmod` succeeds — an exact toolchain-version
match (e.g. Microsoft's vendor gcc 13.2.0 + binutils 2.41) was tried and confirmed **not**
necessary; the struct-module-size mismatch was `CONFIG_DEBUG_INFO_BTF_MODULES` silently
dropping out of `.config` for want of `pahole`, not a compiler-codegen divergence. Full
diagnosis in `kernel-module/baud-enforced/BUILD.md`.

### Rebuilding `kvm_intel.ko` itself (enforced-regime RDTSC patch)

`kernel-module/baud-enforced/rdtsc-enforce.patch` patches `arch/x86/kvm/vmx/vmx.c` +
`include/uapi/linux/kvm.h` **in the `~/wsl-kernel-src/src` tree above**, not a new sibling module —
see `kernel-module/baud-enforced/ENFORCEMENT_DESIGN.md`. Apply once (idempotent — re-running is a
no-op if already applied) and build the whole in-tree KVM module directory, not just one file:

```
grep -q handle_baud_rdtsc_exit ~/wsl-kernel-src/src/arch/x86/kvm/vmx/vmx.c || \
    patch -p1 -d ~/wsl-kernel-src/src < kernel-module/baud-enforced/rdtsc-enforce.patch
cd ~/wsl-kernel-src/src && KBUILD_MODPOST_WARN=1 make CC=gcc-13 M=arch/x86/kvm modules -j$(nproc)
```

This produces `arch/x86/kvm/{kvm.ko,kvm-intel.ko}` (module names `kvm`/`kvm_intel`). **Never**
`insmod` these over the stock `/lib/modules/$(uname -r)/kernel/arch/x86/kvm/*.ko` files — swap them
in live instead, and always swap back:

```
fuser /dev/kvm && echo "REFUSE — a guest is using /dev/kvm" # must print nothing
echo baud | sudo -S rmmod kvm_intel && echo baud | sudo -S rmmod kvm
echo baud | sudo -S insmod ~/wsl-kernel-src/src/arch/x86/kvm/kvm.ko
echo baud | sudo -S insmod ~/wsl-kernel-src/src/arch/x86/kvm/kvm-intel.ko
# ... run whatever needs the patched module ...
echo baud | sudo -S rmmod kvm_intel && echo baud | sudo -S rmmod kvm
echo baud | sudo -S modprobe kvm_intel   # restores the stock module + its kvm.ko dependency
```

`drive/h3-enforced-rdtsc.sh` does exactly this dance (build → swap → run the `#[ignore]`d
`rdtsc_enforced_regime_is_bit_exact_across_boots` test → swap back, unconditionally via a
`trap ... EXIT`) — `drive/h3-enforced-rdrand.sh` is its sibling for RDRAND (applies
`kernel-module/baud-enforced/rdrand-enforce.patch` on top of the same tree, idempotent same as
`rdtsc-enforce.patch`). Every other `drive/*.sh`/`cargo test --workspace` assumes the **stock**
module, so these two are the only scripts that should ever touch the live `kvm_intel`/`kvm`
modules.

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
