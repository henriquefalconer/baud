# baud_enforced_probe — build/load notes

This is the first real, on-hardware artifact toward the "enforced" determinism regime
(todo.md §3.8, specs/baud-multiverse.md §3.8, specs/baud-host.md §8) — the sole item this
project's todo.md has flagged as "not attempted" across many iterations. It does not
implement enforcement; it only answers the open hardware question of whether this host's
VMX microcode even allows the two VM-execution controls the enforced regime needs
(RDTSC-exiting, RDRAND/RDSEED-exiting) to be turned on at all. It never writes VMX state
and never touches a running vCPU — read-only capability-MSR probing only.

## Building

The stock WSL2 kernel (`uname -r` = `6.18.33.2-microsoft-standard-WSL2` on this dev
machine) ships no `linux-headers-*` package and no `/lib/modules/$(uname -r)/build` —
out-of-tree modules cannot be built against it out of the box. To build one:

```
mkdir -p ~/wsl-kernel-src && cd ~/wsl-kernel-src
git clone --depth 1 --branch linux-msft-wsl-6.18.33.2 \
    https://github.com/microsoft/WSL2-Linux-Kernel.git src
cd src
rm -rf .git   # a shallow clone defeats scripts/setlocalversion's tag lookup (Microsoft's
              # tags aren't named like mainline vX.Y.Z, so `git describe` never resolves
              # cleanly) and it always appends a spurious "+" to kernelrelease, which then
              # fails vermagic matching at insmod time. This tree is a one-off scratch
              # build tree, not something that needs its own version control.
zcat /proc/config.gz > .config      # the exact config this running kernel was built with
make CC=gcc-13 olddefconfig         # gcc-13, not the system default — see "Known blocker" below
make CC=gcc-13 modules_prepare -j$(nproc)
sudo ln -sfn "$PWD" "/lib/modules/$(uname -r)/build"

cd /path/to/baud/kernel-module/baud-enforced
KBUILD_MODPOST_WARN=1 make CC=gcc-13   # see below for why KBUILD_MODPOST_WARN is needed
```

`KBUILD_MODPOST_WARN=1` is needed because `modules_prepare` alone (no full `vmlinux`
build) never produces `Module.symvers`, so modpost can't resolve ordinary exported kernel
symbols like `printk` at build time — it still resolves correctly at `insmod` time against
the real running kernel's symbol table. This is standard for headers-only WSL2 module
builds, not specific to this module.

## Known blocker: insmod fails with a struct-module-size mismatch

The module builds cleanly (correct vermagic, matching `uname -r` exactly) but
`insmod baud_enforced_probe.ko` still fails:

```
module baud_enforced_probe: .gnu.linkonce.this_module section size must match the
kernel's built struct module size at run time
```

This is `kernel/module/main.c`'s `elf_validity_cache_index_mod()` rejecting a `sizeof(struct
module)` mismatch between the module and the running kernel. It is **not** a `.config`
problem — a full diff against `/proc/config.gz` (the running kernel's real build config)
shows no relevant option differences once `CC=gcc-13` is used (matching `CONFIG_CC_VERSION_TEXT`'s
major version removes the `CONFIG_CC_HAS_COUNTED_BY` divergence that a stock `gcc-15` build
introduces). It persisted even with `gcc-13.4.0` (Ubuntu's package) standing in for the
kernel's actual `gcc (GCC) 13.2.0` build — the `.config` also still shows
`CONFIG_GCC_ASM_GOTO_OUTPUT_BROKEN=y` on the real running-kernel build vs.
`CONFIG_CC_HAS_ASM_GOTO_OUTPUT=y` with any Ubuntu-packaged gcc-13, and the real build used
`GNU ld 2.41` vs. this machine's `2.46`. Reproducing Microsoft's exact `struct module` ABI
needs their literal build toolchain (exact `gcc 13.2.0` + `binutils 2.41`), not merely a
same-major-version substitute — an open-ended toolchain-matching problem, not a bug in this
module or its Makefile.

**Not yet done**: getting `insmod` to actually succeed (needs the exact vendor toolchain,
e.g. building it from source or finding a matching prebuilt), and — a materially bigger
task after that — implementing the actual enforcement (hooking KVM's own VMCS setup to
force the RDTSC/RDRAND/RDSEED-exiting bits on for every guest; this module only *reads*
the capability MSRs today, matching the spec's "hardware-feasible" open question, not the
feature itself).
