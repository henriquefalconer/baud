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

**At the start of every task run `echo baud | sudo -S sh -c 'echo -1 > /proc/sys/kernel/perf_event_paranoid'`** — it
resets to `2` on every WSL boot, and until it is `-1` every KVM run fails with `failed to create the work-clock's
perf_event branch counter: Permission denied`.

`/dev/kvm` is present on this machine. Confirm it and grant access once:

```
ls -l /dev/kvm && grep -c vmx /proc/cpuinfo      # device exists; VT-x count > 0
sudo usermod -aG kvm "$USER"                      # open /dev/kvm without sudo (re-login after)
cargo run -p baud-cli -- host probe --json        # regime must NOT be "rejected"
```

H1+ (booting a real guest) runs here directly, e.g. `bash drive/h/h1.sh`. If `/dev/kvm` is ever missing,
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

`drive/manual/h3-enforced-rdtsc.sh` does exactly this dance (build → swap → run the `#[ignore]`d
`rdtsc_enforced_regime_is_bit_exact_across_boots` test → swap back, unconditionally via a
`trap ... EXIT`) — `drive/manual/h3-enforced-rdrand.sh` is its sibling for RDRAND (applies
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

**To test baud, run `bash drive/gate.sh`** — one command, the whole gate (build, clippy, workspace tests, and
every drive script), with a per-unit PASS/FAIL table and logs under `target/gate-logs/`.

**If the gate's only failure is `rdtsc_guest_reproduces_high_bits_across_boots` and phase 6 reports it
passing in isolation (unit marked `FLAKE`), the gate counts as passing — commit, do not re-run it.**

**Under `claude -p` never start the gate with `run_in_background`** — ending the turn exits the
process and kills every background task with it (measured: a 45s background job from a `-p` session
that ended its turn at 6s never completed). Run it in the foreground with `timeout: 600000`.

A second `ralph/ralph` plus `claude -p` pair in `ps` during a ralph iteration is the loop that launched the
session, not a competing run — do not investigate it as contention.

```
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

Drive scripts live under `drive/h/`, `drive/m/`, `drive/pkg/`, `drive/manual/` and `drive/demo/`; `gate.sh`
and its `gate.test.bats` sit at `drive/` root. Each builds only what it needs and runs the CLI against a
locally-spawned `baud-server` on an ephemeral port and a temp SQLite file — see any of them for the pattern
(spawn server, health-poll, `trap cleanup EXIT`/`INT`/`TERM`, run `baud <cmd> --json`, assert on the JSON).
The `drive/manual/*` scripts swap the live `kvm_intel` module, so they are excluded from the gate and must
be run by hand, one at a time.

When starting a `baud-server`/`baud` CLI pair by hand (not via a drive script): the server binds via
`BAUD_ADDR` (e.g. `127.0.0.1:17734`) and needs `BAUD_DB="sqlite://<path>?mode=rwc"` (a bare path 404s
with "unable to open database file") plus `BAUD_SNAPSHOT_STORE=<dir>`; the **CLI** then needs
`BAUD_SERVER=http://127.0.0.1:17734` to find it — `BAUD_ADDR` is the server's own bind var, the CLI
never reads it, and defaults to `http://127.0.0.1:7734` if unset. `examples/ubuntu/BUILD.md` has the
full real-Ubuntu-boot recipe (needs `bash examples/ubuntu/fetch.sh` first, artifacts land outside the
repo in `~/.baud-tmp/ubuntu-1804` per the same convention as `~/wsl-kernel-src`).

## Subagents

- Freely use any models when spawning subagents, except for model `gpt-5.6-sol`.
- Never include `fork_context: true` or any `fork_context` parameter on your subagent tool calls.

## Unslop

Edit text to remove AI patterns and add human voice.

### Process

1. Scan for the patterns below.
2. Rewrite. Preserve meaning, match intended tone.
3. Add soul (see next section).
4. Self-audit: "What makes this obviously AI generated?" Fix remaining tells.

### Adding soul

Removing patterns is half the job. Sterile, voiceless writing is just as obvious.

- **Have opinions.** React to facts instead of neutrally listing pros and cons.
- **Vary rhythm.** Short sentences. Then longer ones that take their time. Mix it up.
- **Acknowledge complexity.** "Impressive but also kind of unsettling" beats "impressive."
- **Use "I" when it fits.** First person isn't unprofessional.
- **Let some mess in.** Perfect structure looks machine-made.
- **Be specific.** Not "this is concerning" but "there's something unsettling about agents churning away at 3am."

### Patterns to detect and fix

#### Content

1. **Puffery.** "pivotal moment", "testament to", "evolving landscape", "setting the stage for", "indelible mark", "deeply rooted". Cut puffery, state what happened.
2. **Name-dropping.** Listing media outlets without context. Pick one, say what was said.
3. **Superficial -ing phrases.** "highlighting...", "ensuring...", "reflecting...", "showcasing...", "fostering...". Delete or expand with real sources.
4. **Promotional language.** "nestled", "vibrant", "breathtaking", "groundbreaking", "renowned", "stunning", "must-visit". Use neutral descriptions.
5. **Vague attributions.** "Experts believe", "Industry reports suggest", "Some critics argue". Name the source or delete.
6. **Formulaic challenges.** "Despite challenges... continues to thrive." Replace with specific facts.

#### Language

7. **AI vocabulary.** Additionally, crucial, delve, enduring, enhance, fostering, garner, interplay, intricate, landscape (abstract), pivotal, showcase, tapestry (abstract), testament, underscore, vibrant. Replace with plain words.
8. **Fancy ways to say "is".** "serves as", "stands as", "boasts", "features". Just say "is" or "has".
9. **"Not just X, but Y."** State the point directly instead.
10. **Rule of three.** Forcing ideas into groups of three. Use the natural number.
11. **Synonym cycling.** Protagonist, main character, central figure, hero all in one paragraph. Pick one, repeat it.
12. **False ranges.** "from X to Y" where X and Y aren't on a meaningful scale. List topics directly.

#### Style

13. **Em dash overuse.** Avoid em dashes entirely. Use periods or commas only (no parentheses, no en dashes, no hyphen-as-dash substitutes). Em dashes are an AI tell, and reaching for parentheses instead just trades one tell for another. If a thought needs separation, end the sentence or use a comma.
14. **Colon overuse.** Colons are fine before a list or example. Not as mid-sentence connectors. "If you're coming from traditional automation: instead of registering event handlers, you describe conditions" adds nothing with the colon. Rewrite to let the point stand on its own without comparison framing. "Describing when the scheduler should fire works best as plain English." Same meaning, no crutch punctuation.
15. **Boldface overuse.** Don't bold every proper noun or acronym.
16. **Inline-header lists.** The tell is a bold label and colon that restates the line: "**Performance:** Performance improved...". Convert those to prose. A bold lead-in that ends in a period, names the item, and is followed by genuinely new detail ("**Schema in TypeScript.** Tables live in one file.") is fine, not a tell.
17. **Title case headings.** Use sentence case.
18. **Decorative emojis.** Remove from headings and bullets.
19. **Curly quotes.** Replace with straight quotes.

#### Communication artifacts

20. **Chatbot phrases.** "I hope this helps!", "Let me know if...", "Of course!", "Certainly!", "Found the smoking gun!" Remove.
21. **Cutoff disclaimers.** "While specific details are limited..." Find sources or remove.
22. **Sycophantic tone.** "Great question! You're absolutely right!" Respond directly.

#### Filler

23. **Filler phrases.** "In order to" becomes "To". "Due to the fact that" becomes "Because". "It is important to note that" gets deleted.
24. **Excessive hedging.** "could potentially possibly be argued that it might" becomes "may".
25. **Generic conclusions.** "The future looks bright." State specific plans or facts.

#### Jargon

26. **Abstract metaphor nouns.** Substrate, wedge, vector, locus, vantage, nexus, primitive (as noun), harness (as metaphor), surface (as in "API surface"), bedrock, scaffolding (as metaphor), modality, paradigm, gold-plating, ratchet (as metaphor), evacuate (for moving code), endgame, north star, flywheel. These read as technical but usually have a plainer concrete word. "Substrate" becomes "base". "Wedge in" becomes "add". "Vector" becomes "way" or "method". "Gold-plating" becomes "more than the job needs". "Ratchet" becomes the mechanism's real name or "a limit that only tightens". "Evacuate" becomes "move out". "Endgame" becomes "the last phase". Pick the concrete word.

#### Plain speech

27. **Say what it does, not how it feels.** "the database stays close at hand", "SQL you can read", "types that follow your schema" name a feeling. The fix names the mechanism or a number: "`.toSQL()` returns the exact string sent to the database", "a column rename fails the build". Ask what the sentence tells the reader to do or know, then write that. If you can't restate it as a concrete instruction, fact, or number, cut it. One more check: if the sentence could appear unchanged in another project's docs, it says nothing about this one. Cut it.
28. **Shorten or split dense sentences.** If the reader has to backtrack to parse a sentence, break it in two or drop clauses. One idea per sentence.
29. **Active voice.** Prefer it. Catch "is/are/was/were + past participle" and name the actor: "queries are validated" becomes "the compiler validates queries", "the file is parsed by the loader" becomes "the loader parses the file". Passive is fine only when the actor is unknown or genuinely doesn't matter.
30. **Cut adverbs, or use a stronger verb.** "runs quickly" becomes "is fast" or the number. "significantly improves" becomes the measured delta. An adverb propping up a weak verb means the verb is wrong.
31. **Prefer the plain word.** "utilize" becomes "use", "leverage" becomes "use", "facilitate" becomes "help", "numerous" becomes "many", "in the event that" becomes "if". The fancier synonym is rarely clearer.
