# BAUD — Implementation Plan

**Deterministic-validation infrastructure for distributed systems.**

A deterministic supervisor (`baud-multiverse`) is the foundation and the first deliverable: it runs guest programs such that execution is a pure function of (binary, manifest, tape). On top of it: Daytona sandboxes as disposable "tapes," a Hegel-like external driver built from scratch, pure fuzzing guided by user-specified strategy & tactics, two independent observation planes (supervisor syscall log + eBPF) streamed to a local `baud-server`, and journal-only reconstruction of any instance.

Validated bottom-up against workloads specified purely through the CLI — no workload-specific code in baud crates:
1. a seeded-PRNG hello program, then the "fuzzers hate it" parser (both nix-built static binaries);
2. a 3-node target with a planted "modal distributed-systems bug";
3. finally Super Mario Bros on a single-threaded NES emulator core **running as a guest under baud-multiverse** — the emulator receives no trust; the supervisor enforces determinism on it like on any guest.

This plan is self-contained for a fresh Claude session. Every milestone ends with a drive script that validates the functionality just built; once baud-server exists (M0), all prior functionality is re-driven and tested through the server via the CLI.

---

## 0. Context and goal

- A cluster of communicating processes becomes a **pure function from an input byte stream (the tape) to observable state**: every source of nondeterminism — syscall results, time, entropy, message ordering, delays, drops, partitions, crash/restart schedules, external input — is decided by draws from the tape, mediated by the supervisor.
- Fuzz that stream toward a strategy goal; journal everything; destroy the sandbox at any time and reconstruct the run from the server-side journal alone.
- Why this finds distributed-systems bugs: the modal bug needs leader-election × mid-rollback × failure — sequential luck with multiplied probabilities. Guided exploration (strategy feedback + reservoir/grid) plus stateful fault distributions (network weather, not white-noise packet drops) collapses one-in-a-billion into thousands of trials. This infrastructure demonstrates that collapse at small scale.
- Prior-art anchor (documented in `docs/determinism.md`): Antithesis's hypervisor is a modified bhyve on Intel VMX, one physical core per instance, virtual time pegged to instruction counts, PMC-based interrupt injection. Their published experience — instruction counters miscount ~1 in 10¹² instructions; interrupt delivery lands dozens of instructions late with variable latency — is the engineering cost of supporting *arbitrary threaded software*. baud avoids that entire cost class by constraining guests (single-threaded, syscall-boundary switching) instead of counting instructions.

## 1. Hard constraints

- Daytona sandboxes: **1 vCPU, 1 GiB RAM, 1 GiB disk** (if the API rejects 1 GiB disk, use the platform minimum and record the actual value in the run manifest), **auto-stop = 1 minute, auto-archive = 5 minutes**. These timers are a design forcing function: sandboxes are cattle; anything not journaled server-side within seconds is presumed lost. An entire guest cluster lives inside ONE sandbox, under ONE supervisor.
- `baud-server` runs on the local dev machine (macOS, Apple Silicon). Sandboxes are Linux x86_64 → the supervisor and eBPF run only there; logs are forwarded out.
- ALL interaction happens through the `baud` CLI (server lifecycle included), `--json` on every command; the CLI exposes every piece of information the system holds, including the supervisor syscall log and eBPF logs of any baud-tape.
- Rust workspace; every crate lives in its own directory under `crates/`, prefixed `baud-`; crates communicate only through `baud-proto` types and network/process boundaries.
- Workload names (mario, nes, emulator, raftlet, joypad, …) may appear only under `examples/`, `drive/`, and `docs/` — enforced by a CI grep over `crates/baud-*/src` from M0 on. Workload meaning enters the system as data (specs, adapters, probe names), never as code.
- **Custom engineering is spent only where determinism demands it.** baud owns, from scratch, the parts on which determinism rests: baud-multiverse (supervisor + device models), baud-driver, baud-proto, baud-journal. Every other crate is the thinnest possible shell over an audited building block — vetted crates for cryptography (`ed25519-dalek`, `jsonwebtoken`), the installed `sops`/`age` binaries for secrets, `nix` for builds, `aya` for eBPF. baud never implements cryptography or reinvents a solved primitive; a crate that would require doing so is out of scope.

## 2. Vocabulary

- **guest**: a process under the supervisor's control. Single-threaded, statically linked, built by nix.
- **supervisor**: baud-multiverse — mediates every interaction between guests and the world.
- **tape (data)**: the choice sequence — append-only byte stream of every random decision (syscall scheduling, entropy, input bytes, message delivery, weather, crash/restart). The sole source of nondeterminism.
- **baud-tape (instance)**: a Daytona sandbox provisioned with a workload spec + agent, currently playing tapes.
- **node**: one guest in a multi-guest workload.
- **weather**: the virtual network's stochastic condition (partition on/off, delay regime, drop regime) — stateful and bursty; per-packet coin flips exist only as an explicitly requested negative control.
- **probe**: a named observation extractor (guest stdout key-value, virtual-fs file, syscall-log derived counter, eBPF counter, final-state hash).
- **strategy**: an objective computed from probes (progress metric; optionally lexicographic; optional grid buckets; optional goal predicate).
- **tactics**: the stochastic processes that extend tapes — input generation AND weather AND crash/restart schedules.
- **run**: {seed, workload spec hash, closure hash, strategy, tactics, journal}. Fully describes and reproduces everything.

## 3. baud-multiverse — the deterministic supervisor (first deliverable)

The foundation. A userspace supervisor for Linux guests, running inside the sandbox, owning every guest↔world interaction.

**Guest contract (enforced, not requested — violations kill the guest at the offending instruction, with a report):**
- one thread, one process per guest: `clone`, `fork`, `vfork`, `execve` (post-start) → kill;
- no async signal delivery; only synchronous faults;
- statically linked, no-PIE, musl-built via nix; fixed argv/env/locale from the manifest;
- `personality(ADDR_NO_RANDOMIZE)`; brk/stack layout recorded in the manifest;
- syscalls outside the allowlist → kill with report (never silently emulated).

**Nondeterminism sources and their handling (the requirements table — normative):**

| Source | Handling |
|---|---|
| Thread/process scheduling | Eliminated: one thread per guest; cross-guest switching only at syscall boundaries, order chosen by draws |
| Async signals/interrupts | Eliminated: none delivered |
| Clocks (`clock_gettime`, `gettimeofday`, `nanosleep`, …) | Virtual clock device; time advances deterministically per syscall and per scheduling quantum |
| `rdtsc`/`rdtscp` | `prctl(PR_SET_TSC, PR_TSC_SIGSEGV)` → trap → emulate from virtual clock (works Intel & AMD, works in containers) |
| `cpuid` | `arch_prctl(ARCH_SET_CPUID, 0)` → trap → synthetic fixed CPUID (Intel CPUs); on CPUs without CPUID faulting: record all CPUID leaves + CPU vendor/model in the manifest, pin reconstruction to the same CPU class, double-run verification as backstop |
| `rdrand`/`rdseed` | Feature bits masked in synthetic CPUID; direct use without CPUID check is caught by double-run verification |
| Entropy (`getrandom`, `/dev/urandom`, `AT_RANDOM` auxv) | Served from tape draws, including auxv random bytes at exec |
| Filesystem | In-memory read-only snapshot + copy-on-write; writes hashed into observations; no real disk I/O from guests |
| Network | Virtual socket device: all connect/send/recv mediated; delivery order, delay, drop, duplicate, partition are draws (weather) |
| External input (stdin, fifo bytes) | Tape-fed input channel device |
| Other syscall results (pids, uids, `uname`, `sysinfo`, `/proc`) | Virtualized fixed values from a ~25-syscall allowlist; everything else killed |
| CPU/FP/microarchitectural variation | CPU class + CPUID leaves recorded in manifest; reconstruction requires the same class; double-run verification as backstop |

**Mechanism**: seccomp user-notify for the allowlist (supervisor serves syscalls from device models) + ptrace for trap handling (TSC/CPUID emulation, kill-with-report). Device models — `clock`, `entropy`, `fs`, `input`, `net`, `exit` (final-state hash) — are modules of this crate, each consuming draws via baud-proto types.

**Determinism claim**: single-threaded guests + no async delivery + all syscalls served deterministically + trapped TSC/CPUID + fixed memory layout ⇒ execution is a pure function of (binary, manifest, tape). The claim is verified, not assumed: `verify determinism` (double run, byte-identical observation streams) gates every workload, and CI runs it on the supervisor's own test guests. Contract violations the supervisor cannot trap (CPU-class drift, RDRAND misuse on non-faulting CPUs) surface as a reported first-divergent-step, and the run is marked unusable for replay/shrink/reconstruct.

**Multi-guest clusters**: N guests under one supervisor, one virtual clock, one net device. Guests execute one at a time, switched at syscall boundaries; the switch order is a draw — schedule exploration comes free as tactics. A guest spinning without syscalls starves the cluster; the supervisor detects quantum overrun (wall-clock watchdog, outside the deterministic boundary) and kills with report.

**Observation plane 1 — the syscall log**: the supervisor records every syscall with arguments, result, and virtual timestamp. This is the primary observation stream: probes can be derived from it, and it is journaled like all observations.

**Crate spec**: deps = {libc/nix, seccomp bindings, baud-proto}; no tokio (single-threaded event loop); public API: `Hyper::load(manifest) → run(tape_source) → ObservationStream`; soft budget ≤ 4,000 LOC; the supervisor never interprets guest semantics — it knows syscalls, not workloads.

## 4. Crate map (remaining crates)

- **`baud-proto`** — wire & domain types (serde + CBOR, versioned): `RunManifest`, `ChoiceChunk`, `Observation`, `ProbeSpec`, `StrategySpec`, `TacticsSpec`, `TapeStatus`, `NodeSpec`, `NetEvent`, `DrawRequest/DrawResult`, `SyscallRecord`, `EbpfRecord`, `FrameRecord`.
  - Deps = {serde, ciborium} only; no IO, no async, no chrono (time = `u64` virtual steps); soft budget ≤ 700 LOC.
  - Probe values are opaque `{name: String, value: Value}` — no domain enums. Messages carry a version byte and tolerate unknown fields.
  - The observation vocabulary is exactly: `Observe`, `Crash{node?, invariant?, signal?, detail}`, `GoalReached{metric}`. There are no temporal or formula types; properties beyond crash/invariant/goal (e.g., "eventually", time-bounded response) are out of the protocol by construction — a harness that needs them encodes an invariant or goal probe instead.
- **`baud-driver`** — the Hegel-like engine (do NOT import Hypothesis).
  - Seeded ChaCha PRNG → draw API: `draw_bits(n)`, `draw_int(lo,hi)`, `draw_choice(weights)`, `draw_hold(geom_mean)`, `draw_weather(markov_params)`.
  - Corpus: current-best tape + reservoir of earlier prefixes with backoff probability + optional grid bucketing over discretized probe tuples.
  - Scheduler: extend-best / mutate / splice-from-reservoir. Shrinker: passes over the choice sequence — chunk deletion, zeroing, hold-shortening, dedup; for clusters this shrinks the fault schedule (deliverable = the smallest ops + faults sequence reproducing a violation).
  - Pure library — no IO, no tokio, no threads; deps = {rand_chacha, serde}; public API: `Driver::new(seed, strategy, tactics)`, `next_draw()`, `report_observation()`, `shrink()`. Property test (M3 gate): same seed + same observation replies ⇒ byte-identical tape.
  - Sees only bytes in (draws) and `{probe name → numeric value}` out (scores); it cannot name a button, packet, or node. Strategies and tactics arrive as data (`StrategySpec`/`TacticsSpec`), never as code.
- **`baud-server`** — local daemon.
  - axum on localhost only; storage = SQLite (metadata) + flat content-addressed files (journal); REST + SSE; every endpoint exists because a CLI subcommand needs it, 1:1.
  - A run is `{spec hash, closure hash, seed, strategy, tactics, journal refs}`; the server stores and serves probe streams, syscall logs, and eBPF streams as named series without interpreting workload semantics.
  - Owns reconstruction and the sandbox-minute budget. All functionality of every other component is driven and tested through this server via the CLI once it exists (M0 onward; H-phase functionality is re-driven in M3).
- **`baud-cli`** — the single `baud` binary.
  - Thin client — zero business logic; each subcommand = one server call + one formatter (human table / `--json` passthrough); no local state beyond server address + auth.
  - No workload subcommands; workloads are addressed via `--spec <path>`. Demo sugar lives in `drive/*.sh`.
- **`baud-tape`** — typed REST client for the Daytona API.
  - Wraps only the endpoints baud calls — create/start/stop/archive/delete sandbox, exec, file upload/download, preview URL — enumerated in the crate README; retries with backoff; recorded-fixture contract tests.
  - Hidden behind the `Backend` trait (create/destroy/exec/put/get/status/endpoint) shared with `baud-tape-local`; nothing above the trait may import this crate.
- **`baud-tape-local`** — the `Backend` trait as a local Linux subprocess in a temp dir (on macOS dev machines: a lima/colima VM documented in `doctor`). Exists so CI and integration tests run without cloud or cost. One shared conformance test suite runs against both backends; a feature that works on only one backend fails CI.
- **`baud-tape-agent`** — static musl x86_64-linux binary (cross-built from macOS via nix devshell + cargo-zigbuild) running inside the sandbox: executes baud-init provisioning, launches **baud-multiverse** with the guest set, mediates protocol draws between server and supervisor, applies input adapters, samples probe adapters, streams observation batches out (WebSocket over Daytona preview URL, token-authenticated; fallback: batch files via exec/file API), loads baud-tracing.
  - Binary size budget ≤ 10 MiB; no shell-outs except `nix build`; supervises children with `fork/exec` + pidfd.
  - Contains no workload logic — everything it does to a workload is declared in the spec's adapters; a new workload kind must require zero agent changes (tested by M8: the Mario spec runs on the agent binary built at M2, unmodified).
- **`baud-init`** — declarative first-boot provisioning: YAML user-data → provision steps.
  - Exactly five directive kinds — `nix` (flake ref), `files` (fixtures), `env`, `nodes` (topology: name, argv, adapter bindings), `adapters`; idempotent; unknown directives are hard errors.
  - Adapters are the only extension point, a closed set with strict schemas:
    - input adapters: `stdin`, `fifo{path}`, `net` (via the multiverse net device);
    - probe adapters: `stdout-kv{prefix?}`, `vfs-file{path, mode: hash|u64|utf8}`, `syscall-counter{sysno|pattern}`, `ebpf-counter{event}`, `exit-hash`;
    - display adapters: `frame{width, height, format: rgba8888|rgb565|indexed8, transport: fifo|vfs}` (consumed by baud-stream).
  - This closed set is what keeps workload crates out of the tree: the parser is `stdin` + `stdout-kv`; raftlet is `net` + `stdout-kv`; Mario is `fifo` + `stdout-kv`. The menu grows only by schema-reviewed entry.
- **`baud-packages`** — workload specs as NixOS building blocks: TOML → generated pinned flake → static no-PIE musl guest binaries + fixtures, built in-sandbox (or restored from a prebaked Daytona snapshot image with warm /nix/store for the 1-minute economics). Closure hash in the manifest.
  - One flake template + substitution — no nix-language AST manipulation; wraps `nix build` and `nix copy` only; pinned nixpkgs rev lives in exactly one place.
  - Any nixpkgs-expressible derivation that satisfies the guest contract is a valid workload; baud-packages has no opinion about what it is.
- **`baud-journal`** — journal schema + reconstruction. **Depends on baud-keys; journal contents are encrypted at rest.**
  - Append-only CBOR chunk files + blake3 content addressing — no database, no compaction; readers are streaming iterators; indexes only `(run, step)`.
  - Stores opaque probe values, draw bytes, syscall records, eBPF records.
  - **Encryption at rest (required)**: a journal reproduces the entire deterministic execution — tapes, inputs, and every observation — so a leaked journal directory would reproduce any secret the workload processed. Therefore every chunk is written age-encrypted:
    - baud-journal obtains the age **recipient** (public key) and **identity** (private-key path) from baud-keys — the same age key baud-keys already resolves per-OS (`SOPS_AGE_KEY_FILE` or the OS default). baud-journal calls baud-keys for key resolution and calls the `age` primitive to encrypt/decrypt; it owns no cryptography of its own.
    - **Content addressing is computed over plaintext** (`blake3(plaintext)`), and the ciphertext is stored under that address. Because age encryption is non-deterministic (random ephemeral key per chunk), addressing over plaintext is what preserves deduplication ("identical chunks stored once") and lets verification stay meaningful.
    - **Verification and reconstruction hash plaintext**: observation-stream-hash equality is computed over decrypted plaintext, never over stored ciphertext, so encryption does not perturb the determinism check. Readers decrypt via the baud-keys identity as they stream.
    - Only the `(run, step)` index and chunk addresses are stored in clear; chunk *bodies* are always ciphertext. A missing/incorrect age identity makes a journal unreadable — the same key that guards `secrets/baud.enc.yaml` guards the journal; `doctor` checks it.
  - Reconstruction = `(manifest + tape prefix) → fresh sandbox → same closure → replay under the supervisor → verify observation-stream-hash prefix equality → resume`. Replay cost is O(steps in prefix); there is no mid-run state snapshot — resuming at step K always replays 0..K. Shrinking therefore batches many candidate tapes inside one sandbox process (never one sandbox per trial).
  - Divergence detection reports the first mismatching step and the node/probe/syscall that diverged; a divergent run is marked and excluded from replay/shrink/reconstruct.
  - The reconstruction code path references only manifest fields and hashes; M6/M8 drive scripts assert that raftlet and Mario reconstruct through the identical `tape reconstruct` command.
- **`baud-tracing`** — observation plane 2: kernel-side ground truth.
  - aya-based, fixed prebuilt CO-RE probe set — no BPF compilation inside sandboxes: sched events, exec, syscall entry/exit for supervisor and guests, page faults; ringbuf → agent → server.
  - Purpose: an independent witness of the same execution the supervisor claims to have mediated. `baud verify observation --run` cross-checks plane 1 vs plane 2 (per-guest syscall counts and sequences must agree); disagreement indicates a supervisor bug or an escaped guest and fails the run.
  - If the sandbox kernel denies BPF (likely on shared container runtimes): degrade to /proc-sampling + strace-shim emitting the SAME `EbpfRecord` schema flagged `source=fallback`; the cross-check still runs. Built at M7, not optional.
  - Events are keyed by `{pid → node-id}` mapping supplied by the agent; the probe set knows processes and syscalls, never workload semantics.
- **`baud-stream`** — graphical-surface capture, fingerprinting, and streaming.
  - A guest that has a graphical interface declares it in its spec via the `frame` display adapter (see baud-init): `frame{width, height, format: rgba8888|rgb565|indexed8, transport: fifo|vfs}`. The guest (or its bridge fixture) writes length-prefixed raw frame byte buffers to the declared transport at each frame boundary — an emulator writes its framebuffer, a TUI program writes its cell grid serialized to bytes, any program writes whatever byte surface its spec declares. The multiverse's device model delivers those buffers to this crate on the agent side.
  - Ingest: validate each buffer against the declared width×height×format length; a mismatched frame is rejected and reported as `Crash{detail: "frame-format", node, step}`.
  - Fingerprint: blake3 hash per frame; emit `Observe{probe: "<node>.frame_hash", value: hash}` at the frame's virtual step. Frame hashes participate in double-run verification (frame streams must match bit-for-bit) and are usable as strategy inputs (e.g., `buckets = ["probe:n0.frame_hash"]` explores by distinct screens).
  - Storage discipline: during fuzz runs, **only hashes are journaled — never pixel bytes**. Pixels are regenerated on demand: `baud stream render` replays the tape prefix under the multiverse with capture enabled and materializes the frames. A run's video costs tape-sized storage, and any step of any journaled run can be rendered after the fact. Rendered frames are stored content-addressed (identical frames stored once; long runs of unchanged screens collapse).
  - Encoding: implement the QOI encoder in-crate (~300 lines, zero deps) for single frames, and a Y4M (YUV4MPEG2) writer for frame sequences — a raw, pipeable format so users can produce mp4 with their own ffmpeg; baud takes no codec dependency.
  - Live view: replay-with-capture (and fuzz runs started with `--stream`) forward `FrameRecord`s through the agent's existing transport to the server, which re-serves them over SSE; `baud stream tail` writes Y4M to stdout or a file.
  - Deps = {baud-proto, blake3}; QOI and Y4M writers in-crate; soft budget ≤ 1,200 LOC. The crate knows byte surfaces, dimensions, and formats — never what is depicted.
- **`baud-secret`** — a type-safe wrapper for sensitive values that prevents accidental exposure through logs, serialization, or debug output.
  - `Secret<T: Zeroize>` with `SecretString = Secret<String>` and `const REDACTED = "[REDACTED]"`. No `Deref`: the inner value is reachable only via an explicit `.expose(&self) -> &T`, so every read is visible in code review. Zeroized on drop. `Debug`/`Display`/`Serialize` all emit `[REDACTED]`; `Deserialize` loads normally.
  - File-convention loader: `load_secret_env(var)` checks `{VAR}_FILE` (read the secret from that path, strip one trailing newline) before `{VAR}`, returning `Option<SecretString>`; `require_secret_env(var)` errors when absent.
  - Primitive crate with no config, network, or workload knowledge; deps = {serde, zeroize}; soft budget ≤ 400 LOC. Every other crate that carries a token (baud-keys, baud-tape, baud-identity, baud-tape-agent) holds it as `SecretString`, never `String`.
- **`baud-identity`** — workload identity. Mints ed25519-signed JWTs — signing via `ed25519-dalek`, encode/verify via `jsonwebtoken`; baud owns the token subject scheme, TTL policy, and verification rule, never the cryptography. The server is the sole trust root. Token subject `baud://tape/<sandbox-id>/run/<run-id>[/node/<i>]`; TTL 10 minutes, renewed before expiry; derived per-node identities attribute observations. Every agent connection presents a token verified against the root public key; unauthenticated connections are refused (preview URLs are public). Tokens held as `SecretString`.
- **`baud-keys`** — provider secrets at rest. Wraps the installed `sops` and `age` binaries by shelling out (baud owns no cryptography): one age-encrypted `secrets/baud.enc.yaml` holds the Daytona API key and the identity root key. `baud keys init|edit|show --redacted|rotate` map to the corresponding `sops` operations. Resolves the age key from `SOPS_AGE_KEY_FILE` or the OS default (`~/Library/Application Support/sops/age/keys.txt` on macOS, `~/.config/sops/age/keys.txt` on Linux); `doctor` verifies both binaries are installed and the key path is correct per-OS. Depends on baud-secret: decoded values are handed out as `SecretString`, never bare strings. Secrets never land on sandboxes — tapes see only minted identity tokens.
- **`baud-raftlet`** — a validation *target* (a program under test, not infrastructure): a 3-node leader-election + replicated-log toy, each node a single-threaded guest speaking via the net device, with a planted safety violation (two nodes commit different values at the same index) reachable only via leader-election × in-flight-truncation × second-partition. Invariant checks (single leader per term, log prefix agreement) run in-harness and report `Crash{invariant}`.
  - < 1,000 LOC, no deps beyond baud-proto; deployed via `examples/raftlet/spec.toml` through the same CLI, adapters, and code paths as every other workload.

## 5. The protocol (Hegel-like, built from scratch)

- Core inversion: the driver is the source of all randomness; the supervisor's device models *request* draws; recorded results ARE the tape; replay = feed the tape back; shrink = edit the tape and replay.
- Messages (CBOR): `Hello{identity, manifest_hash}`, `DrawRequest{kind, bounds}`, `DrawResult{bytes}`, `Observe{probe_id, node, value, step}`, `SyscallRecord{node, sysno, args_digest, ret, vtime}`, `EbpfRecord{...}`, `FrameRecord{node, step, width, height, format, hash, bytes?}` (bytes absent in hash-only mode), `Checkpoint{stream_hash, step}`, `GoalReached{metric}`, `Crash{node?, invariant?, signal?, detail}`, `Eof`.
- `baud verify determinism` = same seed, two fresh tapes, byte-identical observation stream hashes across the whole workload. Runs are gated on a passing verification for the spec; a failure reports the first divergent step.
- `baud verify observation` = supervisor syscall log vs eBPF stream cross-check for a run.

## 6. Strategy & tactics specification (CLI-first)

- `StrategySpec` (TOML or inline flag):
  - `maximize = "probe:<name>"` or lexicographic `maximize = ["probe:a", "probe:b", "probe:c"]`;
  - optional `buckets = ["probe:a", "probe:b"]` → grid exploration over incomparable dimensions;
  - optional `reservoir = { keep = 32, p_backoff = 0.1 }`;
  - optional `goal = "probe:<name> == <value>"` → emits `GoalReached`, exit code 2 semantics.
- `TacticsSpec` built-ins:
  - input tactics: `random` (white-noise negative control), `stateful-mask{p_flip}` (previous input byte remembered, bits flipped with low probability), `hold{geom_mean}`;
  - weather tactics: `markov-partition{p_start, p_stop}`, `burst-delay{regimes}`, `crash-restart{p, min_up_ticks}`;
  - schedule tactics: `switch-bias{weights}` (cross-guest switch-order distribution);
  - weighted composition of any of the above.
- Stretch (post-M9): WASM escape hatch — user module exporting `score(observations) -> f64` and `mutate(prev, rand) -> bytes`.

## 7. Milestones — H-series (hypervisor first), then M-series (infrastructure), each with a drive script in `drive/`

- **H0 — capability spike** (timeboxed, one day, in a real Daytona sandbox): ptrace? seccomp user-notify? `PR_SET_TSC`? `ARCH_SET_CPUID` (CPU vendor survey across sandbox creations)? kernel version? Results recorded in `docs/determinism.md`; the CPUID path (faulting vs record-and-pin) is chosen here.
  - Drive `drive/h0.sh`: runs the probe binary via Daytona exec, prints the capability report.
- **H1 — supervisor MVP**: static hello-world guest under full mediation (allowlist, virtual clock, fs snapshot, entropy from a fixed byte source).
  - Drive: run the same guest twice → byte-identical observation stream hashes; run a `clone()`-calling guest → killed at the offending syscall with report; run a guest using an unmodeled syscall → killed with report; run an `rdtsc`-calling guest → trapped and served virtual time.
- **H2 — tape integration**: device models consume draws; the seeded-PRNG hello workload, then the hello/goodbye parser with a planted crash behind `while(true)` (`examples/parser/`), fuzzed under the supervisor with a local stand-in driver loop.
  - Drive: `random` tactics plateau on a depth probe; `stateful-mask` penetrates; crash found; the crashing tape replays to the same crash.
- **H3 — multi-guest + net device**: N guests, one virtual clock, syscall-boundary switching by draws, weather draws.
  - Drive: two echo guests exchange messages deterministically; double-run equality holds for a 3-guest topology under `markov-partition` weather.
- **M0 — server + CLI bootstrap**: baud-proto, baud-server, baud-cli skeletons, baud-keys; workload-noun CI grep lands.
  - Drive: `keys init` → `server start|status|logs` → `doctor`.
- **M1 — backends & tape lifecycle**: `Backend` trait, local backend, Daytona client, identity; backend conformance suite lands.
  - Drive: `tape create` on both backends → `status` shows 1vCPU/1GiB/1GiB + 1m/5m → `exec echo` → observe the 1-minute auto-stop fire → `ensure` revives → post-archive `ensure` restores → `kill`.
- **M2 — provisioning**: baud-init (five directives + adapters) + baud-packages; hello-deterministic workload spec end-to-end onto a tape, guest running under baud-multiverse via the agent.
  - Drive: `spec lint` → `run start` provisions and executes → `obs tail` shows probe values → `run status` shows closure hash.
- **M3 — journal + replay through the server**: draws journaled, replay path, `verify determinism` as a CLI command; H1/H2 functionality re-driven through the server. Driver determinism property test gates this milestone.
  - Drive: `verify determinism --spec hello-deterministic` passes → a `time()`-poisoned variant fails with first-divergent-step report → `replay` reproduces a prior run exactly.
- **M4 — fuzz loop through the server**: strategy/tactics/corpus/goal detection on `examples/parser/spec.toml`.
  - Drive: `--tactics random` plateaus in `run watch` → `--tactics stateful-mask` + depth strategy → exit code `2`; the winning tape journaled.
- **M5 — multi-guest and stream through the server**: H3 functionality (topology in specs, weather tactics) plus baud-stream, driven via CLI. Validation workload `examples/framedemo/`: a small C guest that writes a moving gradient as `indexed8` frames to its `frame` adapter for N virtual steps.
  - Drive: 3-guest echo topology; `net weather --run` prints the recorded partition/delay timeline; double-run equality via `verify determinism` including frame-hash streams; `stream frames` lists hashes; `stream render --format y4m` materializes the gradient sequence and a re-render is byte-identical; `stream tail` shows live frames during a `--stream` run.
- **M6 — raftlet**: `examples/raftlet/spec.toml` with the planted modal bug.
  - Drive: `run start --tactics random-drops` → invariant never trips within budget → `run start` with markov weather + crash-restart + grid strategy → `Crash{invariant: log_prefix_agreement}` within budget → exit `2` → `net weather` shows the causal partition timeline → mid-run `tape kill` + `tape reconstruct` + resume.
- **M7 — eBPF plane + cross-check**: prebuilt CO-RE probes, fallback path, `verify observation`.
  - Drive: `tracing tail` streams live during an M4 run → `tracing summary` → `verify observation --run` passes on a healthy run → a deliberately broken supervisor build (test fixture) fails the cross-check → BPF-denied sandbox exercises `source=fallback` visibly.
- **M8 — Mario under the hypervisor** (spec-only; `examples/mario/`): a single-threaded, headless NES emulator core packaged via nix as a guest binary satisfying the guest contract (fceux and other threaded emulators do not qualify — evaluate single-threaded cores; if none fits the contract, build a bare NES core as a workload fixture, still outside baud crates). ROM and savestate are user-supplied paths (never bundled; CI uses a homebrew ROM). Controller bytes via `fifo` input adapter; probes via `stdout-kv` from a bridge fixture: `x_page`=mem[0x006D], `x`=mem[0x0086], `x_global`, `y`=mem[0x00CE]→`y_band`, `world`=mem[0x075F], `level`=mem[0x075C], `lives`, `game_over`, `game_completed`.
  - Strategy: lexicographic `maximize = ["probe:world", "probe:level", "probe:x_global"]`, `buckets = ["probe:x_page", "probe:y_band"]`, `reservoir = {keep=32, p_backoff=0.1}`, `goal = "probe:game_completed == 1"`. Tactics: `stateful-mask{p_flip=0.03}`; `random` as negative control.
  - The emulator bridge declares a `frame{256, 240, indexed8, fifo}` display adapter exposing the NES framebuffer.
  - Drive `drive/m8.sh`: lint → `verify determinism` → negative control plateau → main run climbing worlds/levels → mid-run kill + reconstruct + resume → `GoalReached{game_completed}` → shrink → `replay` of the shrunk tape still completes → `stream render --run <winning-run> --format y4m -o mario-completion.y4m` produces the watchable completion video from the tape alone. Budget parameterized (default 600 minutes; CI variant accepts `world ≥ 2`; full completion is the release gate). The agent and supervisor binaries used are the ones built at M2 — unchanged.
- **M9 — hardening & demo**: budget accounting, docs (`determinism.md`, `protocol.md`), `drive/full-demo.sh` chaining every CLI command.

## 8. CLI surface (complete reference)

- `baud server start|stop|status|logs [--follow]`
- `baud doctor` — env checks: sops/age binaries + age key path, Daytona reachability, cross toolchain, local backend VM
- `baud keys init|edit|show --redacted|rotate`
- `baud spec new|lint|show <spec.toml>`
- `baud tape create|ls|status|ensure|kill|reconstruct|exec|probe-caps <id>`
- `baud run start --spec S --strategy ST --tactics T --seed N --budget-minutes M` ; `run ls|status|watch|pause|resume|abort <run>`
- `baud obs ls|get|tail --run <id> [--probe X] [--node I] [--json]`
- `baud syscalls tail|get --run <id> [--node I] [--sysno N]` — the supervisor's syscall log
- `baud tracing tail --tape <id> [--event sched|syscall|exec|fault] [--node I]` ; `baud tracing summary --run <id>`
- `baud net weather --run <id>` — partition/delay timeline as recorded on the tape
- `baud stream tail --run <id> [--node I] [-o out.y4m] [--hashes-only]` — live frames (or frame-hash timeline) over SSE
- `baud stream render --run <id> [--from-step A --to-step B] [--format qoi-seq|y4m] -o PATH` — replay with capture, materialize frames
- `baud stream frames --run <id> [--node I]` — list journaled frame hashes by step
- `baud verify determinism --spec S --seed N [--times 2]`
- `baud verify observation --run <id>` — syscall-log vs eBPF cross-check
- `baud shrink <run> [--passes chunk-delete,zero,hold-shorten]` → smallest tape + fault schedule report
- `baud replay <run> [--tape-file F] [--to-step K]`
- `baud budget`
- Global: `--json` everywhere. Exit codes: `0` completed, `1` error, `2` goal/violation found.

## 9. Risks & pre-made decisions

- **ptrace/seccomp-unotify denied in Daytona containers**: H0 decides. If ptrace is denied, TSC/CPUID trapping fails → fall back to seccomp-only mediation + guest contract forbidding rdtsc/cpuid (checked by static binary scan at spec lint) + double-run backstop. If user-notify is denied, ptrace-only syscall interception (slower, same semantics).
- **AMD sandboxes (no CPUID faulting)**: record-and-pin path from §3; H0 surveys the fleet.
- **eBPF denied** (likely on shared container runtimes): fallback shim mandatory at M7; cross-check runs against it identically.
- **1 GiB disk vs nix store**: prebake a Daytona snapshot image with warm /nix/store, built reproducibly by `infra/pkgs/baud-sandbox-image.nix` (§11); if minimum disk > 1 GiB, accept minimum and record deviation.
- **No single-threaded NES core satisfies the guest contract**: build a bare core as a workload fixture under `examples/mario/` (it is a target, not infrastructure); timeboxed decision at M8 start.
- **ROM copyright**: never fetch or bundle; ROM/savestate are user-supplied spec parameters; homebrew ROM in CI.
- **Guest spin-loops** (no syscalls → starvation): supervisor wall-clock watchdog (outside the deterministic boundary) kills with report; documented in the guest contract.
- **macOS dev machine cannot run the supervisor locally**: local backend uses a lima VM (checked by `doctor`); CI runs Linux natively. macOS is never a NixOS host — `infra/` modules/machines (§11) govern Linux only (the sandbox image and any CI host); locally we stay on the Nix devshell + lima VM.
- **macOS → linux cross-compile of agent/supervisor**: built reproducibly via the `infra/pkgs` fenix overlay (§11) targeting static musl, replacing ad-hoc `cargo-zigbuild`; `doctor` validates the cross toolchain.
- **Preview-URL/WS blocked**: transport falls back to exec+file polling automatically; same CBOR batches.
- **Daytona API drift**: isolated in baud-tape; recorded-fixture contract tests.

## 10. Repo layout

```
Cargo.toml                             # workspace; members = crates/*
Cargo.lock                             # committed (this workspace ships binaries)
.gitignore
flake.nix                              # dev shell + exposes infra/pkgs outputs
crates/
  baud-multiverse/  baud-proto/  baud-driver/  baud-server/  baud-cli/
  baud-tape/  baud-tape-local/  baud-tape-agent/  baud-init/  baud-packages/
  baud-journal/  baud-tracing/  baud-stream/  baud-secret/  baud-identity/  baud-keys/
  baud-raftlet/                    # a TARGET program, deployed via examples/raftlet spec
infra/                               # see §11
  secrets/      # multi-recipient sops: .sops.yaml, dev.yaml, ci.yaml, update-secret, .gitignore, README
  pkgs/         # nix overlay: cross-built agent/supervisor/guests + baud-sandbox-image (OCI)
  nixos-modules/# baud-sandbox.nix, baud-host.nix, security-audit.nix, nix-settings.nix
  machines/     # sandbox.nix (the Daytona image), ci.nix (continuous-testing host)
examples/
  hello-deterministic/  parser/      # H2/M2/M4 workload specs
  framedemo/                         # M5 display-adapter validation guest
  raftlet/                           # spec.toml for the raftlet cluster
  mario/                             # spec.toml, bridge fixture, strategy.toml, README (no ROM!)
drive/          # h0.sh … h3.sh, m0.sh … m9.sh, full-demo.sh
docs/           # determinism.md, protocol.md
specs/          # baud-*.md — one component specification per crate
```

---

## 11. Infrastructure (`infra/`)

Nix-native provisioning, adapted to baud's reality: **baud-server runs locally on macOS, and "machines" are ephemeral Daytona sandboxes that never hold secrets.** So `infra/` borrows the build, secrets, and image patterns of a NixOS fleet, but not the persistent-host lifecycle (no k3s/podman/web/auto-update on sandboxes). Everything under `infra/` governs Linux only; the macOS dev box stays on the devshell + lima VM.

### 11.1 `infra/secrets/` — multi-recipient sops

Upgrades today's single-key `secrets/baud.enc.yaml` to a multi-recipient, per-environment layout so several developers and CI can each decrypt without sharing one private key.

```
infra/secrets/
  .sops.yaml            # named age recipients (dev keys, CI key) + per-file creation_rules
  .gitignore            # deny-all allowlist: only the encrypted *.yaml + tooling are committed
  README.md             # proprietary header
  secrets.yaml.example  # plaintext template of the key names
  dev.yaml              # encrypted: daytona api_key/api_url, github token, cachix token, baud identity root key
  ci.yaml               # encrypted: CI-scoped subset (daytona key + homebrew ROM path)
  update-secret         # decrypt → $EDITOR → re-encrypt → verify; the impl behind `baud keys edit`
```

- `.sops.yaml` names each recipient as an anchor and encrypts each file to the union of relevant keys (least privilege — CI never sees dev-only secrets):
  ```yaml
  keys:
    - &dev_you   age1...          # your key (already generated)
    - &ci_runner age1...          # CI's host key via ssh-to-age
  creation_rules:
    - path_regex: dev\.yaml$
      key_groups: [ age: [ *dev_you ] ]
    - path_regex: ci\.yaml$
      key_groups: [ age: [ *dev_you, *ci_runner ] ]
  ```
- **SSH-host-key → age (`ssh-to-age`)** lets an unattended CI/staging host decrypt its own `ci.yaml` at deploy time with no human present — the enabler for the "continuous cron testing" idea. It applies ONLY to trusted baud-server/CI hosts; **never to Daytona sandboxes** (secrets never touch tapes; tapes see only minted identity tokens).
- Ciphertext `*.yaml` IS committed; private keys and any decrypted material never are (root `.gitignore` + the nested allowlist enforce this).
- Wires to `baud-keys` (§ spec): a `--env` selector (`dev.yaml` default, `ci.yaml` for CI); `baud doctor` checks `sops`/`age`/`ssh-to-age` and the recipient set.

### 11.2 `infra/pkgs/` — Nix build + the Daytona sandbox image (build first)

A fenix-based cross-compilation overlay that de-risks two items §9 currently leaves open (the snapshot image and the macOS→linux cross-build).

```
infra/pkgs/
  default.nix              # overlay; fenix cross toolchains (macOS host → linux musl targets)
  baud-agent.nix           # static musl x86_64 baud-tape-agent (replaces ad-hoc cargo-zigbuild)
  baud-multiverse.nix      # the supervisor, static musl
  baud-guests.nix          # example guests (parser, raftlet) as pinned static binaries
  baud-sandbox-image.nix   # dockerTools OCI image: agent + supervisor + tracing probes + warm /nix/store
```

- `baud-sandbox-image.nix` (via `dockerTools.buildImage`) **is** the "prebaked Daytona snapshot with warm /nix/store" from §9 — now reproducible, with a closure hash that slots into `baud-packages`' environmental-determinism story.
- The overlay replaces the informal devshell `cargo-zigbuild` path; it is what `baud doctor`'s cross-toolchain check validates.
- **This is the first `infra/` piece to build** — H-phase agent/supervisor cross-builds and M2 provisioning both consume it.

### 11.3 `infra/nixos-modules/` + `infra/machines/` — composable sandbox & CI definitions

Composable `{ config, lib, pkgs }` modules (options + config), only the few that fit baud:

```
infra/nixos-modules/
  baud-sandbox.nix     # image contents: agent, supervisor, seccomp policy, warm store
  baud-host.nix        # a Linux host running baud-server on a cron for continuous testing
  security-audit.nix   # auditd rules (execve/syscall + /run/secrets watches)
  nix-settings.nix     # pinned nixpkgs, gc, Cachix substituters
infra/machines/
  sandbox.nix          # composes baud-sandbox + nix-settings → the Daytona snapshot
  ci.nix               # composes baud-host + security-audit + secrets(ci.yaml) → continuous-testing host
```

- `security-audit.nix` (auditd with `execve`/syscall rules) is a **kernel-independent third observation source**, complementing baud-multiverse's syscall log (plane 1) and baud-tracing's eBPF (plane 2). Borrow it for the CI host's baud-server; `baud-tracing` (§ spec) references it as a coarse fallback where eBPF is denied.
- `ci.nix` operationalizes "cron job to continuously run longer tests on main/staging"; a GitOps auto-update loop (poll repo → rebuild → activate) may run **here only**, never on sandboxes.

### 11.4 Deliberately dropped

`k3s`, `podman`, web, `smtprelay`, `maxmind`, `tailscale`, `fail2ban`, and auto-update-on-sandboxes — all assume a persistent, networked, multi-service SaaS host. baud sandboxes are single-purpose cattle built from an image; the local server is macOS. Adopting these would be infrastructure baud does not have.

### 11.5 Build order

`infra/pkgs` (unblocks H1 cross-builds + M2 image) → `infra/secrets` (multi-recipient, once a second dev/CI exists) → `infra/nixos-modules` + `infra/machines` (when the CI/continuous-testing host is wanted). Nix is already on baud's critical path via `baud-packages`, so `infra/` adds no new dependency class.

---

## 12. Pending items — verification round 1 triage (2026-07-24)

Items are ordered: blockers first, then major, then minor. Each item is scoped to approximately one build iteration. Spec citations and repro notes are included inline.

---

### BLOCKERS

#### VR1-B1: baud-multiverse crate is entirely absent

The core deliverable does not exist. No `crates/baud-multiverse/` directory, not listed in `Cargo.toml` workspace members. No seccomp user-notify code, no ptrace trap handler, no virtual clock device model, no entropy/fs/net/input/exit device models, no allowlist enforcement, no guest process launcher. The public API (`Multiverse::load(manifest, guests) -> Result<Self>` and `run(&mut self, tape: impl DrawSource) -> ObservationStream`) has zero implementation. The H-series and M2–M9 milestones all claim to run guests through this supervisor — none of them do.

- Spec: `specs/baud-multiverse.md:39-48` (Crate Architecture), `:98-103` (API), `:129-148` (§8 Testing); `todo.md:45-81` (§3 baud-multiverse)
- Repro: `ls crates/` — no `baud-multiverse`; `grep -r "seccomp\|ptrace\|PR_SET_TSC\|ARCH_SET_CPUID" crates/ --include="*.rs"` returns nothing.
- Scope: scaffold the crate, implement seccomp user-notify syscall interception + ptrace trap handler for TSC/CPUID + the six device models (clock, entropy, fs, input, net, exit) + the allowlist enforcer + the guest launcher; wire up `Multiverse::load` / `run` returning a real `ObservationStream`.

#### VR1-B2: H-series drive scripts (h0.sh–h3.sh) are absent

`drive/h0.sh`, `drive/h1.sh`, `drive/h2.sh`, `drive/h3.sh` do not exist. `drive/` contains only `m0.sh–m8.sh` and `full-demo.sh`. The H-series milestones (H0 capability spike, H1 supervisor MVP, H2 tape integration, H3 multi-guest + net device) each have a normative drive script per the plan.

- Spec: `todo.md:175` (H0 drive), `:178` (H1 drive), `:180` (H2 drive), `:182` (H3 drive); `todo.md:263` (drive/ repo layout listing h0–h3)
- Repro: `ls drive/` — h0–h3 absent.
- Scope: create `drive/h0.sh` through `drive/h3.sh` validating the corresponding supervisor milestones once VR1-B1 lands.

#### VR1-B3: Supervisor normative tests absent (double_run_is_bit_identical, clone_syscall_is_killed, rdtsc_is_trapped)

Section 8 of `specs/baud-multiverse.md` defines three named Rust tests with exact assertions. None exist anywhere in the codebase. The H1 exit criterion is "the double-run test passes on a static hello guest."

- Spec: `specs/baud-multiverse.md:129-148` (§8 Testing); `todo.md:77` (H1 exit criterion)
- Repro: `grep -rn "double_run_is_bit_identical\|clone_syscall_is_killed\|rdtsc_is_trapped" crates/` — no results.
- Scope: add `#[test] fn double_run_is_bit_identical`, `fn clone_syscall_is_killed`, `fn rdtsc_is_trapped_and_served_virtual_time` to `crates/baud-multiverse/` once VR1-B1 scaffolds the crate.

#### VR1-B4: verify determinism is tautological — no real supervisor execution

`POST /verify/determinism` calls `generate_deterministic_observations(seed, spec_hash, spec_doc)` twice with identical arguments. The function is a pure mathematical formula (DefaultHasher, no guest, no supervisor, no tape replay). Both runs trivially produce identical output by construction. A comment at line 83 of `crates/baud-server/src/routes/verify.rs` explicitly acknowledges this: "For M3, we implement the verification harness: generate observations deterministically…"

- Spec: `specs/baud-multiverse.md:119` (§7 Determinism Claim), `:129-132` (§8 double-run test); `todo.md:75` ("The claim is verified, not assumed"); `todo.md:189` (verify determinism as CLI command)
- Repro: `curl -s -X POST http://localhost:3000/verify/determinism -H 'Content-Type: application/json' -d '{"spec":"hello-deterministic","seed":42}' | jq .` — always returns `ok:true` regardless of workload.
- Scope: wire `verify/determinism` to run the workload through `baud-multiverse` twice and compare real observation-stream hashes. Depends on VR1-B1.

#### VR1-B5: baud-tape-agent crate is absent from the workspace

No `crates/baud-tape-agent/` directory exists; not listed in `Cargo.toml` members. The spec defines it as the static musl x86_64-linux binary running inside the sandbox that launches `baud-multiverse`, mediates protocol draws, streams observations. The spec test `unmodified_agent_runs_a_new_workload` does not exist. The M2 milestone requires this binary to be built and provisioned.

- Spec: `todo.md:104-108` (baud-tape-agent spec), `:247` (workspace crate map); `specs/baud-tape-agent.md §2` Crate Architecture, `§5` Testing
- Repro: `ls crates/` — no `baud-tape-agent`; `grep "baud-tape-agent" Cargo.toml` — not listed.
- Scope: scaffold `crates/baud-tape-agent/` as a static musl binary crate; implement provisioning via baud-init, multiverse launch, draw relay, observation streaming; add `unmodified_agent_runs_a_new_workload` test.

#### VR1-B6: baud-journal stores chunk bodies in plaintext — age encryption not implemented

`crates/baud-journal/src/lib.rs` lines 17–20 contain an explicit comment deferring encryption to "M5+." All chunks are written as raw CBOR bytes. The spec-required test `chunk_bodies_are_ciphertext` does not exist. A leaked journal directory reproduces the entire deterministic execution including any secrets the workload processed.

- Spec: `todo.md:120-127` (encryption at rest requirement); `specs/baud-journal.md §3` (Encryption at Rest), `§6` (Testing: `chunk_bodies_are_ciphertext`)
- Repro: `hexdump -C <any journal chunk file>` — readable CBOR, not age ciphertext.
- Scope: integrate `baud-keys` age key resolution into `baud-journal`; encrypt each chunk body with `age` before writing, decrypt on read; content-address over plaintext (blake3); add `chunk_bodies_are_ciphertext` test.

#### VR1-B7: baud-server embeds Mario and raftlet workload code, violating the hard constraint

`crates/baud-server/src/routes/mario.rs` contains ~400 LOC of NES physics simulation (BTN_RIGHT, BTN_LEFT, world/level, joypad bytes). `crates/baud-server/src/routes/raftlet.rs` directly calls `baud_raftlet::simulate()`. `baud-server/Cargo.toml` lists `baud-raftlet` as a direct dependency. The CI grep over `crates/baud-*/src/` for workload nouns (mario, raftlet, joypad, nes) finds 47–50 violations. `full-demo.sh` hides this by narrowing the grep to exclude `baud-server` and `baud-raftlet`.

- Spec: `specs/baud-server.md:28` (Non-Goals: "No workload interpretation"); `todo.md:28-29` (hard constraints: crate communication via baud-proto only; workload-noun CI grep)
- Repro: `grep -rn --include="*.rs" -E '\b(mario|raftlet|emulator|joypad)\b|\bnes\b' crates/baud-*/src/` — exits 1 with 47+ hits. `bash drive/m1.sh` — fails at workload-noun CI grep step.
- Scope: remove `crates/baud-server/src/routes/mario.rs` and `raftlet.rs`; remove `baud-raftlet` from `baud-server/Cargo.toml`; replace embedded simulation with generic fuzz/run endpoints that accept opaque workload specs (the server must store and serve, not interpret). Fix `full-demo.sh` CI grep to use the correct broad pattern.

#### VR1-B8: tape exec bypasses the Backend trait — runs bare sh in server CWD, not sandbox directory

`POST /tapes/{id}/exec` (lines 363–376 of `crates/baud-server/src/routes/tapes.rs`) runs `tokio::process::Command::new("sh").arg("-c").arg(&shell_cmd).output()` with no cwd override. Every exec runs in the baud-server process directory. The `LocalBackend` created during tape creation is discarded immediately. Sandbox isolation for exec is completely absent.

- Spec: `specs/baud-tape-local.md:56` (exec: Run argv in the sandbox dir); `specs/baud-tape.md:62` (Backend trait `exec` method)
- Repro: create two tapes, exec `pwd` on each — both return the same working directory (`/Users/vm/code/baud`).
- Scope: persist the `Backend` instance per tape ID (or re-create it from stored metadata); route exec through `backend.exec(id, argv)` so that local tapes run in the temp sandbox dir.

#### VR1-B9: baud-keys secrets_file() hardcodes wrong path ('secrets/baud.enc.yaml')

`crates/baud-keys/src/lib.rs:82` returns `PathBuf::from("secrets/baud.enc.yaml")`. The actual encrypted secrets file is at `infra/secrets/baud.enc.yaml`. `doctor()` always reports `secrets_file_exists: false`. Any call to `decrypt_secrets(&secrets_file())` fails with file-not-found.

- Spec: `specs/baud-keys.md:60-84`
- Repro: `baud doctor --json | jq .secrets_file_exists` → `false`. `ls secrets/baud.enc.yaml` → no such file; `ls infra/secrets/baud.enc.yaml` → exists.
- Scope: change `secrets_file()` to return `PathBuf::from("infra/secrets/baud.enc.yaml")` (or derive from workspace root).

#### VR1-B10: baud-keys decrypt_secrets cannot extract nested YAML — daytona_api_key() always returns MissingKey

The encrypted secrets file has a nested YAML structure (`daytona: { api_key: ... }`). After sops decryption, `decrypt_secrets` iterates only top-level object keys and calls `v.as_str()`, which returns `None` for nested objects. The nested values are silently dropped. `daytona_api_key()` calls `secrets.require("daytona_api_key")` and always returns `KeysError::MissingKey`.

- Spec: `specs/baud-keys.md:65-72`
- Repro: with a valid age key and `infra/secrets/baud.enc.yaml` decryptable, calling `baud keys show --redacted` reports MissingKey for daytona_api_key and identity_root_key.
- Scope: fix `decrypt_secrets` to flatten nested YAML objects using dotted-key or structured extraction (e.g., `daytona.api_key` → `"daytona_api_key"`); update `require()` call sites to match the flattened key names.

#### VR1-B11: Planted bug in baud-raftlet is a no-op — both conditional branches are identical

Lines 257–265 of `crates/baud-raftlet/src/lib.rs`: the `planted_bug_active` conditional has `term >= self.current_term` in both branches. The comment says the bug branch should use `>=` and the normal branch should use `>`, but neither branch uses `>`. The flag has zero effect on cluster behavior. The M6 drive script finds a violation through a different unintended mechanism, not through the planted bug path.

- Spec: `specs/baud-raftlet.md:56-58` (Section 3: The Planted Bug)
- Repro: set `planted_bug_active = false`; run the M6 markov-crash-restart guided run — the `log_prefix_agreement` violation is still found because the identical bug paths make the two modes indistinguishable.
- Scope: fix the conditional so the normal branch uses strict `>` (reject stale leaders) and the bug branch uses `>=` (accept stale leaders, overwriting committed entries).

---

### MAJOR

#### VR1-M1: baud-proto missing proptest suite (cbor_roundtrips, unknown_trailing_field_still_decodes)

Spec §6 requires two proptest-based property tests over arbitrary `Msg` and `Observation` values. Neither exists. The implementation has only 6 hand-written unit tests. Additionally, `decode_lenient` and `with_extra_field` helper functions required by the spec tests are not implemented.

- Spec: `specs/baud-proto.md:138-149`
- Repro: `grep -rn "proptest\|cbor_roundtrips\|unknown_trailing" crates/baud-proto/` — no results.
- Scope: add `proptest` dev-dependency; implement `decode_lenient` and `with_extra_field`; add `cbor_roundtrips` and `unknown_trailing_field_still_decodes` property tests.

#### VR1-M2: baud-proto no golden vectors checked in

Spec §6 states: "Golden vectors: fixed CBOR byte strings checked in, so wire drift is caught." No golden vector fixtures exist anywhere in `crates/baud-proto/`.

- Spec: `specs/baud-proto.md:152`
- Repro: `ls crates/baud-proto/tests/` — no fixture files.
- Scope: serialize one canonical value of each `Msg` variant; check in the hex-encoded CBOR bytes as test fixtures; add a test that re-decodes them and asserts structural equality.

#### VR1-M3: baud-proto no length caps on collection fields (security §8)

Spec §8 states "Bounded decoders; length caps on collection fields" as the mitigation for hostile/oversized CBOR. Vec fields (e.g., `bytes` in `ChoiceChunk`/`DrawResult`/`Value::Bytes`, `argv` in `NodeSpec`, `maximize`/`buckets` in `StrategySpec`) have no length limits. Unbounded CBOR inputs can cause OOM.

- Spec: `specs/baud-proto.md:165-168`
- Repro: craft a CBOR map with a `bytes` field containing 1 GiB of zeros; deserializing it allocates the full buffer.
- Scope: add a serde `deserialize_with` wrapper or a manual `Deserialize` impl that rejects any collection field exceeding a specified cap (e.g., 64 MiB for byte arrays, 1024 entries for string lists).

#### VR1-M4: baud-secret missing expose_mut, into_inner, PartialEq, and Eq

Spec §3 API table defines `expose_mut(&mut self) -> &mut T`, `into_inner(self) -> T where T: Clone`, and lists `PartialEq`/`Eq` as key properties. None are implemented. The struct derives only `Clone` and `ZeroizeOnDrop`.

- Spec: `specs/baud-secret.md:93-103`
- Repro: compile a callsite using `secret.expose_mut()` or `Secret::into_inner(s)` — compile error.
- Scope: add `expose_mut`, `into_inner` methods; derive or impl `PartialEq` and `Eq` for `Secret<T: PartialEq>`.

#### VR1-M5: baud-secret load_secret_env returns Option instead of Result<Option, SecretEnvError>

Spec §4 API specifies `fn load_secret_env(var: &str) -> Result<Option<SecretString>, SecretEnvError>`. Implementation at `crates/baud-secret/src/lib.rs:92` returns `Option<SecretString>`. File read failures on `{VAR}_FILE` paths are silently swallowed.

- Spec: `specs/baud-secret.md:119`
- Repro: set `MY_SECRET_FILE=/nonexistent/path`; call `load_secret_env("MY_SECRET")` — returns `None` instead of `Err(SecretEnvError::FileReadError(...))`.
- Scope: define `SecretEnvError` enum; change `load_secret_env` signature to `Result<Option<SecretString>, SecretEnvError>`; propagate IO errors from `_FILE` path reads.

#### VR1-M6: baud-identity missing expired_token_is_refused and wrong_root_key_is_refused tests

Spec §5 requires two security tests. (1) `expired_token_is_refused`: a token minted 11 minutes ago must be rejected by `verify()`. (2) `wrong_root_key_is_refused`: a token minted by one root key must be rejected by a different root's `verify()`. Neither exists. The existing `should_renew_expired` test (line 394) only checks the renewal predicate, not that `verify()` rejects an expired token.

- Spec: `specs/baud-identity.md:82-93`
- Repro: `grep -rn "expired_token_is_refused\|wrong_root_key_is_refused" crates/baud-identity/` — no results.
- Scope: add both tests to `crates/baud-identity/src/lib.rs`; mock or manipulate `issued_at` to simulate an 11-minute-old token; generate two separate `RootKey` instances and verify cross-rejection.

#### VR1-M7: baud-keys edit, show --redacted, and rotate commands not implemented

Spec §4 defines four commands: `baud keys init`, `baud keys edit`, `baud keys show --redacted`, and `baud keys rotate`. Only `init_secrets` is implemented. `edit` (decrypt → `$EDITOR` → re-encrypt → verify), `show --redacted` (print keys with `[REDACTED]` values), and `rotate` (sops rotate to new recipients) are absent.

- Spec: `specs/baud-keys.md:108-111`
- Repro: `baud keys edit` → unimplemented stub; `baud keys show --redacted` → unimplemented stub; `baud keys rotate` → unimplemented stub.
- Scope: implement `edit_secrets` (shell out to sops decrypt → `$EDITOR` → re-encrypt), `show_redacted` (decrypt then replace values with `[REDACTED]`), and `rotate` (sops rotate command); wire them to the CLI.

#### VR1-M8: Error responses return HTTP 200; CLI exits 0 for server-side errors on tape subcommands

Routes like `GET /tapes/{id}` and `DELETE /tapes/{id}` return JSON `{"error": "tape X not found"}` with HTTP 200. The CLI client (`client.rs` lines 28–30) checks HTTP status only; since status is 200 it returns `Ok` and exits 0. By contrast, `run.rs` lines 102–104 manually check `v.get("error")` — but `tape.rs` has no such check.

- Spec: `specs/baud-cli.md:83` (Exit codes: 0 completed · 1 error · 2 goal/violation)
- Repro: `baud tape status nonexistent-tape --json`; observe exit code 0 and `{"error":"tape nonexistent-tape not found"}`.
- Scope: fix error-returning routes to return appropriate HTTP status codes (404, 500, etc.); update CLI client to map non-2xx status to exit code 1.

#### VR1-M9: Backend conformance suite never invoked in any test

`baud_tape::backend::conformance::run_conformance` is defined in `crates/baud-tape/src/backend.rs` lines 77–120 but never called from any `#[test]` in `baud-tape` or `baud-tape-local`. The lifecycle sequence (stop → ensure → archive → ensure → gone) specified in `baud-tape.md §6` is also absent from the suite.

- Spec: `specs/baud-tape.md:92-101` (`backend_conformance_parity`); `specs/baud-tape-local.md:72-76`
- Repro: `grep -rn "run_conformance" crates/` — only the definition, no call sites.
- Scope: add `#[test]` functions in both `baud-tape` and `baud-tape-local` that call `run_conformance` against each backend; extend the suite to cover the full stop→ensure→archive→ensure→gone lifecycle.

#### VR1-M10: Journal encryption deferred — baud-journal plaintext (duplicate of VR1-B6, major angle)

Beyond the blocker, the spec-required test `chunk_bodies_are_ciphertext` is absent and the journal reconstruction path never decrypts. Replay and replay-hash comparison operate on plaintext CBOR, so when encryption lands the reconstruction code must also be updated.

- Spec: `specs/baud-journal.md §6` (Testing: `chunk_bodies_are_ciphertext`)
- Scope: add `chunk_bodies_are_ciphertext` test alongside the encryption implementation (VR1-B6); update the streaming reader and `reconstruct` path to call the baud-keys identity for decryption.

#### VR1-M11: First-divergent-step is hardcoded to 9999, not computed

`determinism_poisoned` endpoint (`crates/baud-server/src/routes/verify.rs` line 203–204) always sets `divergent_step = Some(9999)` without comparing two runs step-by-step.

- Spec: `specs/baud-journal.md §5` (Divergence); `todo.md §3` ("reports the first divergent step")
- Repro: `POST /verify/determinism/poisoned` always returns `divergent_step: 9999`.
- Scope: implement step-by-step comparison of two observation runs; report the index of the first differing step along with the node/probe/syscall identity.

#### VR1-M12: verify determinism and replay use synthetic observations, bypassing real execution

`generate_deterministic_observations` and `generate_replay_observations` in `crates/baud-server/src/routes/verify.rs` and `replay.rs` produce fake observations from `DefaultHasher(seed, spec_hash)`. They bypass `baud-driver`, `baud-journal`, and `baud-multiverse` entirely.

- Spec: `todo.md §7` M3 milestone ("verify determinism passes"; "time()-poisoned variant fails with first-divergent-step report"; "replay reproduces a prior run exactly"); `todo.md §4` baud-driver API spec
- Repro: `baud verify determinism --spec hello-deterministic` returns `ok:true` even if the spec does not exist.
- Scope: wire `verify/determinism` to run the spec through the real driver + supervisor (depends on VR1-B1, VR1-B5); wire `replay` to re-feed the journaled tape through the supervisor and compare observation stream hashes.

#### VR1-M13: NIXPKGS_REV is a branch tag, not a pinned commit hash

`crates/baud-packages/src/lib.rs:34` sets `pub const NIXPKGS_REV: &str = "23.11"`. This is a mutable branch tag; the generated flake pins `nixpkgs.url = "github:NixOS/nixpkgs/23.11"` which upstream can update at any time, destroying reproducibility.

- Spec: `specs/baud-packages.md §1` Goals; `specs/baud-packages.md §3` Spec→Flake
- Repro: the generated flake has `nixpkgs/23.11` not a SHA like `nixpkgs/e96e4ef`.
- Scope: replace `NIXPKGS_REV` with a full commit SHA; update the generated flake template accordingly.

#### VR1-M14: nix copy not implemented in baud-packages

`build_real()` in `crates/baud-packages/src/lib.rs` calls `nix build` and `nix path-info` but never calls `nix copy`. The store-warming step required for 1-minute sandbox economics is absent.

- Spec: `specs/baud-packages.md §3` Spec→Flake, `§4` Economics
- Repro: trace the code in `crates/baud-packages/src/lib.rs:173-222` — no `nix copy` invocation.
- Scope: add a `nix copy --to <store-url> <closure>` call after the successful build path in `build_real()`.

#### VR1-M15: Strategy probe names in baud-raftlet don't match spec names

Spec §5 (`specs/baud-raftlet.md:83`) specifies `maximize = ["probe:op_depth"]`, `buckets = ["probe:leader_count","probe:partition_state","probe:term_band"]`. Implementation exposes probes named `max_commit`, `has_leader`, `max_term`, `partition_active`, `pending_msgs`. The fuzz loop uses `maximize = ["max_commit","max_term"]`, `buckets = ["max_term"]`. None match the spec. `op_depth`, `leader_count`, `partition_state`, `term_band` do not exist.

- Spec: `specs/baud-raftlet.md:83` (Section 5: Strategy & Tactics)
- Repro: `grep -n "op_depth\|leader_count\|partition_state\|term_band" crates/baud-raftlet/src/lib.rs` — no results.
- Scope: rename probe extractors to `probe:op_depth`, `probe:leader_count`, `probe:partition_state`, `probe:term_band`; update the fuzz route `StrategySpec` accordingly.

#### VR1-M16: M6 drive script omits the shrink step required by the spec

`drive/m6.sh` covers negative control, guided run, net weather, tape kill + reconstruct, and replay, but has no step exercising `baud shrink` or the shrink endpoint. Spec §6 of `baud-raftlet.md` includes shrink as a mandatory step in the M6 drive sequence.

- Spec: `specs/baud-raftlet.md:101-103` (Section 6: Testing — M6 drive)
- Repro: `grep -n "shrink" drive/m6.sh` — no results.
- Scope: add a shrink step to `drive/m6.sh` that calls `baud shrink <run>` and verifies the output; add a replay step on the shrunk tape.

#### VR1-M17: draw_weather ignores p_stop, implements stateless not Markov tactic

`draw_weather` in `crates/baud-driver/src/lib.rs` lines 284–290 discards `p_stop` (`let _ = p_stop;`) and uses a stateless per-call coin flip. The stateful Markov logic in the raftlet fuzz loop uses an external `rng` (ChaCha20Rng) not part of the driver tape, so weather decisions are not recorded and are not reproducible via tape replay.

- Spec: `todo.md:168` (TacticsSpec built-ins: `markov-partition{p_start, p_stop}`)
- Repro: call `driver.draw_weather(0.1, 0.1)` twice with the same driver state — the result is independent of previous calls (stateless).
- Scope: track a `partition_state: bool` in `Driver`; use `p_stop` to transition out of the active state; ensure all weather draws are recorded on the tape via the standard `record_draw` path.

#### VR1-M18: cross_check() verifies syscall counts only, not ordered sequences

`cross_check` at `crates/baud-tracing/src/lib.rs:279` computes only per-node total syscall counts (`HashMap<u16,u64>`) and compares them. It never compares the ordered sequence of syscall numbers by virtual time. A supervisor bug swapping the order of two different syscalls would pass undetected.

- Spec: `specs/baud-tracing.md:68-71` (§4 Cross-Check): "Per-guest syscall counts and sequences must agree"; `specs/baud-tracing.md:93-100` (§6 Testing — `planes_agree_on_healthy_run`)
- Repro: feed `cross_check` with plane1 and plane2 containing the same syscall counts but different orderings — returns `Ok(())`.
- Scope: extend `cross_check` to compare per-node ordered `Vec<sysno>` sorted by `vtime`; return a mismatch error if sequences differ.

#### VR1-M19: BpfAvailability::probe() always returns Fallback on Linux — Native path is dead code

`crates/baud-tracing/src/lib.rs:85-104` unconditionally returns `BpfAvailability::Fallback` on both the `cfg!(target_os = "linux")` branch and the else branch. A comment acknowledges that BPF detection is possible but was skipped "for safety." The independent-witness property of plane 2 is not achieved; the cross-check always passes because plane 2 is derived from plane 1 via `synthetic_from_syscalls`.

- Spec: `specs/baud-tracing.md:43-48` (§2 Crate Architecture); `specs/baud-tracing.md:77-83` (§5 Fallback); `todo.md:130-133` (baud-tracing: "independent witness")
- Repro: on Linux, `BpfAvailability::probe()` returns `Fallback`; `grep -n "Native" crates/baud-tracing/src/lib.rs` — variant exists but is never constructed.
- Scope: implement `bpf(BPF_PROG_LOAD, ...)` availability probe on Linux; return `Native` when BPF is available; load prebuilt CO-RE probes via aya on the `Native` path; keep `Fallback` path for denied environments.

#### VR1-M20: baud-stream bad frame geometry returns FrameError, not Outcome::Crash

`crates/baud-stream/src/lib.rs:30-39` and `frame.rs:49-63` return `FrameError::SizeMismatch` — a Rust error type — not `baud_proto::Outcome::Crash`. The spec-required test `bad_geometry_is_a_crash` asserts `matches!(ingest(short_buffer()), Outcome::Crash { .. })`.

- Spec: `specs/baud-stream.md:72-74` (§4 Ingest & Fingerprint); `specs/baud-stream.md:119-121` (§7 Testing — `bad_geometry_is_a_crash`)
- Repro: call `ingest` with a buffer shorter than `width×height×format_bytes` — returns `Err(FrameError::SizeMismatch {...})` not `Outcome::Crash`.
- Scope: change the ingest return type to emit `Outcome::Crash{detail: "frame-format", node, step}` on geometry mismatch; add `bad_geometry_is_a_crash` test.

---

### MINOR

#### VR1-m1: baud-secret Debug format outputs '[REDACTED]' instead of 'Secret("[REDACTED]")'

`crates/baud-secret/src/lib.rs:45` writes `{REDACTED}` (i.e., `[REDACTED]`) not `Secret("[REDACTED]")`. The test at line 124 asserts equality with the `REDACTED` constant (wrong assertion), masking the discrepancy.

- Spec: `specs/baud-secret.md:88`
- Repro: `format!("{:?}", Secret::new("x".to_string()))` → `"[REDACTED]"` instead of `Secret("[REDACTED]")`.
- Scope: fix the `Debug` impl to emit `Secret("[REDACTED]")`; fix the test assertion to compare against the correct string.

#### VR1-m2: baud-secret missing proptest-based debug_never_contains_secret and serialize_never_contains_secret tests

Spec §5 requires two proptest tests over arbitrary secret strings. Only fixed-value unit tests exist.

- Spec: `specs/baud-secret.md:134-147`
- Repro: `grep -rn "debug_never_contains_secret\|serialize_never_contains_secret" crates/baud-secret/` — no results.
- Scope: add `proptest` dev-dependency; add `debug_never_contains_secret` and `serialize_never_contains_secret` property tests.

#### VR1-m3: baud-keys DoctorReport missing ssh-to-age check and recipient check

Spec §3 states `doctor` checks that `sops`, `age`, and `ssh-to-age` are installed, the OS-correct key path exists, and the current key is a recipient of the selected environment file. The `DoctorReport` struct and `doctor()` check only `sops` and `age` binaries plus key path.

- Spec: `specs/baud-keys.md:98`
- Scope: add `ssh_to_age_present: bool` field to `DoctorReport`; add a `is_recipient: bool` check verifying the current age key is listed in the sops recipients of the selected yaml file.

#### VR1-m4: baud-keys missing show_redacted_hides_value and rotate_invalidates_old_key tests

Spec §6 requires `show_redacted_hides_value` (output contains `[REDACTED]`, not the real API key) and `rotate_invalidates_old_key` (decryption with old key fails after rotation). Neither exists.

- Spec: `specs/baud-keys.md:128-138`
- Scope: add both tests once the `show --redacted` and `rotate` commands are implemented (depends on VR1-M7).

#### VR1-m5: StrategySpec is defined with incompatible schemas in baud-driver and baud-proto

`baud-driver/src/lib.rs` defines `StrategySpec` with flat scalar fields (`reservoir_keep: u32`, `reservoir_p_backoff: f64`, `goal_probe: Option<String>`, `goal_value: Option<f64>`). `baud-proto/src/lib.rs` defines a different `StrategySpec` with nested types (`reservoir: Option<Reservoir>`, `goal: Option<Predicate>`). A future `baud-tape-agent` integrating both will encounter a type mismatch.

- Spec: `todo.md:84-85`; `specs/baud-multiverse.md:47`
- Scope: resolve to one canonical `StrategySpec` in `baud-proto`; update `baud-driver` to import and use it.

#### VR1-m6: Top-level 'adapters' directive in baud-init is silently ignored

`SpecDoc` in `crates/baud-init/src/parse.rs` has no `adapters` field. A spec document with a top-level `adapters:` block lints successfully but the content is silently discarded.

- Spec: `specs/baud-init.md §3` Directives
- Repro: write a spec with top-level `adapters:` key; `baud spec lint` passes with no warning.
- Scope: add `adapters` field to `SpecDoc`; parse and validate top-level adapter declarations with the same schema as node-level adapters; add a test for top-level adapter round-trip.

#### VR1-m7: Driver::new missing TacticsSpec parameter from spec API

Spec (baud-driver.md §5 API) defines `Driver::new(seed: u64, strategy: StrategySpec, tactics: TacticsSpec) -> Self`. Implementation has `Driver::new(seed: u64, strategy: StrategySpec)`. The `TacticsSpec` type and tactics tiers are unimplemented.

- Spec: `specs/baud-driver.md §5` (API); `todo.md §4`
- Scope: define `TacticsSpec` struct with built-in tactic variants; add `tactics` parameter to `Driver::new`; use `tactics` in `next_draw()` to select the appropriate draw strategy.

#### VR1-m8: draw_bits returns u64 instead of Vec<u8> as specified

`crates/baud-driver/src/lib.rs:237` returns `u64`. The spec declares `fn draw_bits(&mut self, n: u32) -> Vec<u8>`.

- Spec: `specs/baud-driver.md §3` (Draw API)
- Scope: change `draw_bits` return type to `Vec<u8>`; update all call sites.

#### VR1-m9: Replay verification checks observation count not stream hash equality

`crates/baud-server/src/routes/replay.rs:131` sets `verified = replayed.len() == orig_obs_count || orig_obs_count == 0`. A replay producing different observation values but the same count is falsely reported as verified.

- Spec: `specs/baud-journal.md §4` (Reconstruction): "verify observation-stream-hash prefix equality"; `todo.md §7` M3.4 ("replay: ok=true, replay_stream_hash present")
- Scope: compute a blake3 hash over the ordered observation stream; compare against the stored stream hash from the original run.

#### VR1-m10: baud-server start and stop are no-ops that only print to stderr

`ServerAction::Start` (line 33–36) prints an `eprintln!` and returns `Ok()`. `ServerAction::Stop` (line 37–40) prints and returns `Ok()`. Neither starts nor stops the server process. The `--follow` flag for `baud server logs` is parsed but ignored (route returns a static empty array).

- Spec: `specs/baud-cli.md:55` (baud server start|stop|status|logs [--follow]); `specs/baud-server.md §1`
- Scope: implement `ServerAction::Start` to spawn the `baud-server` binary as a background process (writing PID to a lock file); implement `Stop` to send SIGTERM to the stored PID; implement `--follow` for `logs` via SSE or file tail.

#### VR1-m11: Spec-mandated test names absent from baud-tracing and baud-stream

`specs/baud-tracing.md §6` specifies `planes_agree_on_healthy_run` and `fallback_emits_same_schema`. `specs/baud-stream.md §7` specifies `frame_hashes_double_run_identical`, `render_is_byte_identical`, and `bad_geometry_is_a_crash`. None of these function names exist in either crate.

- Spec: `specs/baud-tracing.md:91-101`; `specs/baud-stream.md:107-122`
- Scope: rename or add test functions to match the spec-mandated names exactly; `bad_geometry_is_a_crash` also requires the type fix from VR1-M20.

---

## 13. Pending items — verification round 2 triage (2026-07-24)

Items are ordered: blockers first, then major, then minor. Each item is scoped to approximately one build iteration. Spec citations and repro notes are included inline.

---

### BLOCKERS

#### VR2-B1: H2 and H3 drive scripts fail — invalid cargo build command

`drive/h2.sh:47` and `drive/h3.sh:45` both run `cargo build -q -p baud-multiverse --bin baud-server --bin baud`. Cargo rejects this because `baud-server` and `baud` bins belong to the `baud-server` and `baud-cli` packages respectively, not to `baud-multiverse`. Both scripts exit 101 immediately with: `error: no bin target named baud-server in baud-multiverse package`. H2 and H3 milestones cannot complete.

- Spec: `specs/baud-multiverse.md:149` (H1/H3 drive scripts); `drive/h2.sh:47`; `drive/h3.sh:45`
- Repro: `bash drive/h2.sh` → exits 101; `bash drive/h3.sh` → exits 101.
- Scope: fix the cargo build command in both scripts to build each binary from the correct package (e.g., `cargo build -q -p baud-multiverse -p baud-server -p baud-cli`).

#### VR2-B2: No ptrace/seccomp-unotify implementation — supervisor is a simulation loop

Spec §5 requires seccomp user-notify for allowlisted syscalls and ptrace for TSC/CPUID emulation and kill-with-report. The only code in `crates/baud-multiverse/src/lib.rs` is a synthetic simulation loop (lines 471–566) that draws fake syscall numbers from the tape and never spawns a real guest process, installs a seccomp filter, attaches ptrace, sets `PR_SET_TSC`, or calls `ARCH_SET_CPUID`. The `libc` crate is declared as a dependency but is never actually used. The comment at lines 444–467 explicitly labels this a simulation mode fallback, but this is the only implementation that exists for all platforms.

- Spec: `specs/baud-multiverse.md:90-95` (§5 Mechanism); `specs/baud-multiverse.md:75-85` (§4 Nondeterminism Handling)
- Repro: `grep -n "seccomp\|ptrace\|PR_SET_TSC\|ARCH_SET_CPUID\|fork\|execve" crates/baud-multiverse/src/lib.rs` — no results.
- Scope: implement real guest launch via `fork/execve` with `personality(ADDR_NO_RANDOMIZE)`; install seccomp user-notify filter; attach ptrace for TSC/CPUID trapping and kill-with-report; wire device models (clock, entropy, fs, input, net, exit) to serve real syscall intercepts. H0 capability spike results in `docs/determinism.md` determine the ptrace vs user-notify path.

#### VR2-B3: M2 drive script fails — workload-noun CI grep catches baud-raftlet/src/lib.rs

`drive/m2.sh` step M2.10 runs `grep -rn --include="*.rs" -E "\b(mario|raftlet|emulator|joypad)\b|\bnes\b" crates/baud-*/src/` and exits 1 because `crates/baud-raftlet/src/lib.rs` contains the word 'raftlet' (lines 4, 85, 658, 756). `todo.md:29` says workload names may appear only under `examples/`, `drive/`, and `docs/` — the CI grep scope includes `crates/baud-raftlet/src/` with no exclusion. Every milestone drive script that runs the workload-noun CI grep will also fail for the same reason.

- Spec: `todo.md:29`; `drive/m2.sh:188-190`
- Repro: `bash drive/m2.sh` fails at M2.10 with `✗ workload noun found in crates/baud-*/src/ — CI grep FAILED`.
- Scope: adjust the CI grep pattern or scope to exclude `crates/baud-raftlet/src/` (which is a target workload, not infrastructure); ensure the exclusion is reflected consistently in all drive scripts and the full-demo.sh check.

#### VR2-B4: baud-tape-agent is a non-functional scaffold

`crates/baud-tape-agent/src/agent.rs` emits `scaffold run complete (no supervisor integration yet)` and does nothing beyond calling `lint()`. Steps 3–7 in `main.rs` (launch baud-multiverse, relay DrawRequest/DrawResult, apply input adapters, sample probe adapters, stream via WebSocket) are all unimplemented. `Cargo.toml` has no `tokio-tungstenite` or `tungstenite` dependency; `transport.rs` only contains `StdioTransport`. `baud-packages` is not a dependency so guest building is absent. The M2 milestone claims to run guests via the agent but the agent does nothing.

- Spec: `specs/baud-tape-agent.md §3` (Responsibilities table); `specs/baud-tape-agent.md §4` (Transport); `specs/baud-tape-agent.md §5` (Testing)
- Repro: run the agent binary — it prints the scaffold message and exits 0 without launching any supervisor.
- Scope: implement the agent's core responsibilities: read Hello from stdin (CBOR), provision via baud-init, launch baud-multiverse with the guest set, relay DrawRequest/DrawResult over the protocol, apply adapters, stream observation batches out via WebSocket; add `unmodified_agent_runs_a_new_workload` test.

#### VR2-B5: tape reconstruct is not implemented — M6 drive fails with non-JSON error

`baud tape reconstruct <id>` prints a plain-text error to stderr (`tape reconstruct <id>: not yet implemented (M6)`) and exits with no JSON output. The M6 drive script at line 177 attempts to parse this as JSON and fails with `JSONDecodeError: Expecting value`. M6 milestone cannot complete. Spec §6 of `baud-raftlet.md` requires mid-run `tape kill` + `reconstruct` + resume as part of the M6 drive.

- Spec: `specs/baud-raftlet.md:101` ('mid-run tape kill + reconstruct + resume'); `crates/baud-cli/src/cmds/tape.rs:112`
- Repro: `bash drive/m6.sh` aborts at M6.7 every time with JSON parse error.
- Scope: implement `tape reconstruct <id>` in the CLI and the corresponding server route; the route should restore a killed tape from the stored manifest and journal prefix, re-provision, and return the new tape ID as JSON.

#### VR2-B6: M6 fuzz loop runs parser simulation, not raftlet — wrong workload exercised

The `POST /runs/fuzz` handler (`crates/baud-server/src/routes/fuzz.rs:396`) always calls `simulate_parser()` regardless of the workload spec passed. `baud-server` has no dependency on `baud-raftlet` in its `Cargo.toml`. Even when a raftlet `spec.yaml` is passed, the fuzz loop exercises the parser. The winning input is a parser crash sequence, not a Raft interleaving. M6 drive passes only because it does not verify which workload was fuzzed.

- Spec: `specs/baud-raftlet.md:85-101` (fuzz finds `Crash{invariant: log_prefix_agreement}`); `crates/baud-server/Cargo.toml` (no `baud-raftlet` dep)
- Repro: `POST /runs/fuzz` with raftlet spec → response shows `winning_input: [0x41, 0x40, 0x60, 0xC4, ...]` (parser bytes, not a Raft message sequence).
- Scope: dispatch the fuzz loop based on the spec workload type; for raftlet specs call `baud_raftlet::simulate()` rather than the parser simulation; add `baud-raftlet` as a dependency of `baud-server` only through a trait boundary to avoid workload-noun violations.

---

### MAJOR

#### VR2-M1: unknown_trailing_field_still_decodes proptest does not exercise forward-compatibility

The spec (`specs/baud-proto.md:143-149`) requires: `fn unknown_trailing_field_still_decodes(o in any::<Observation>()) { prop_assert!(decode_lenient(&with_extra_field(&o)).is_ok()); }`. The implementation at `crates/baud-proto/src/lib.rs:554-562` takes `msg in arb_msg()` (not `o in any::<Observation>()`), and never calls `with_extra_field` inside the proptest body — it only tests `decode_lenient(&encoded)` which is identical to normal decode. Forward-compatibility with future protocol fields is not actually exercised. `with_extra_field` is defined (line 428) but takes `&Msg`, not `&Observation`.

- Spec: `specs/baud-proto.md:143-149`
- Repro: `grep -n "with_extra_field" crates/baud-proto/src/lib.rs` shows the function is defined but never called in the proptest body.
- Scope: fix `unknown_trailing_field_still_decodes` to (a) use `o in any::<Observation>()` as the proptest input type, (b) call `with_extra_field(&o)` inside the body, and (c) assert `decode_lenient` succeeds on the result; update `with_extra_field` signature or add an overload taking `&Observation`.

#### VR2-M2: cbor_roundtrips proptest omits 6 of 9 Msg variants

The `arb_msg()` generator at `crates/baud-proto/src/lib.rs:514-536` only covers `Hello`, `Observe`, `Outcome::Crash`, `Outcome::GoalReached`, and `Eof`. `DrawRequest` (which carries `MarkovParams` with f64 fields), `DrawResult`, `Syscall`, `Ebpf`, `Frame`, and `Checkpoint` are entirely absent. These are the most structurally complex variants with bounded-deserialization fields (bytes, step, hash). The proptest does not verify that the six omitted variants roundtrip correctly under arbitrary inputs.

- Spec: `specs/baud-proto.md:138-141`
- Repro: `grep -n "DrawRequest\|DrawResult\|Syscall\|Ebpf\|Frame\|Checkpoint" crates/baud-proto/src/lib.rs` in the `arb_msg` function — no matches.
- Scope: add strategies for all six missing variants to `arb_msg()`; include appropriate arbitrary generators for `MarkovParams`, byte arrays, hashes, and step counts.

#### VR2-M3: rotate_invalidates_old_key tests data-key rotation, not recipient rotation

Spec (`specs/baud-keys.md:135-138`) specifies: `let f = encrypt_to(key_a()); rotate_to(key_b()); assert!(decrypt_with(key_a(), &f).is_err())` — the old key must fail to decrypt after rotating to a new recipient. The implementation at `crates/baud-keys/src/lib.rs:416-447` calls `rotate_secrets` (sops `--rotate --in-place`, which refreshes the SOPS data encryption key but keeps the same age recipients), then verifies the same age identity can still decrypt. This tests data-key rotation semantics, not recipient rotation. The spec's assertion `decrypt_with(key_a()).is_err()` is never exercised.

- Spec: `specs/baud-keys.md:135-138`
- Repro: after `rotate_secrets()`, decryption with the original age identity still succeeds; the test would pass even if `rotate_secrets` did nothing meaningful.
- Scope: fix `rotate_invalidates_old_key` to (a) encrypt with key A, (b) rotate recipients to key B only, (c) assert that attempting to decrypt with key A returns an error; update `rotate_secrets` to accept a new recipient list and re-encrypt with it.

#### VR2-M4: API signature mismatch — load() drops the guests: Vec<GuestImage> parameter

Spec §5 (`specs/baud-multiverse.md:99-103`) defines `fn load(manifest: RunManifest, guests: Vec<GuestImage>) -> Result<Self>`. The implementation at `crates/baud-multiverse/src/lib.rs:412` only takes `manifest: RunManifest`; the `GuestImage` type does not exist anywhere in the codebase. Guest binaries are embedded inside `RunManifest.guests: Vec<GuestSpec>` instead. This is an observable API deviation from the spec.

- Spec: `specs/baud-multiverse.md:99-103`
- Repro: `grep -rn "GuestImage" crates/` — no results; `grep -n "fn load" crates/baud-multiverse/src/lib.rs` shows single-argument form.
- Scope: define the `GuestImage` type (binary bytes + checksum); add it as the second parameter to `Multiverse::load`; update all call sites including `baud-tape-agent` and the verify route.

#### VR2-M5: clone_syscall_is_killed test verifies allowlist membership only, not kill-with-report

Spec §8 (`specs/baud-multiverse.md:136-139`) requires: `assert!(matches!(hyper.run(guest("calls_clone")).outcome, Crash { detail, .. } if detail.contains("clone")))` — actually running a guest binary that issues clone and confirming the supervisor terminates it with a `Crash` outcome containing "clone" in the detail field. The test at `crates/baud-multiverse/src/lib.rs:677` only calls `m.is_permitted(56)` (allowlist lookup). No guest is launched, no kill-with-report path is exercised. The `Crash` variant and `guest()` helper do not exist in the codebase.

- Spec: `specs/baud-multiverse.md:136-139`
- Repro: `grep -n "clone_syscall_is_killed" crates/baud-multiverse/src/lib.rs` — body contains only `m.is_permitted(56)` with no guest execution.
- Scope: implement a `guest(name: &str)` test helper that loads a pre-built test binary; fix `clone_syscall_is_killed` to launch the `calls_clone` guest binary through `Hyper::run` and assert the `Crash{detail}` outcome contains "clone"; depends on VR2-B2 for real guest execution.

#### VR2-M6: rdtsc test verifies clock arithmetic only, not the TSC trap mechanism

Spec §8 (`specs/baud-multiverse.md:140-143`) requires: `let obs = hyper.run(guest("reads_rdtsc")); assert!(obs.completed() && obs.tsc_reads_are_monotonic_virtual())`. The implementation at `crates/baud-multiverse/src/lib.rs:718` only calls `m.clock.advance(1000)` and checks arithmetic. No guest binary is run, `PR_SET_TSC` is never set, no ptrace SIGSEGV trap is exercised. `ObservationStream` lacks the `completed()` and `tsc_reads_are_monotonic_virtual()` methods required by the spec test.

- Spec: `specs/baud-multiverse.md:140-143`
- Repro: `grep -n "tsc_reads_are_monotonic_virtual\|completed" crates/baud-multiverse/src/lib.rs` — no results on `ObservationStream`.
- Scope: add `completed()` and `tsc_reads_are_monotonic_virtual()` methods to `ObservationStream`; fix `rdtsc_is_trapped` test to launch the `reads_rdtsc` guest and assert these methods return the correct values; depends on VR2-B2.

#### VR2-M7: Wall-clock watchdog for spinning guests not implemented

Spec §6 (`specs/baud-multiverse.md:112-114`) states: 'A guest that spins without syscalls starves the cluster — wall-clock watchdog (outside the deterministic boundary) kills it with a report.' No watchdog timer, thread, or `SIGALRM` mechanism exists anywhere in `crates/baud-multiverse/src/lib.rs`. Since there are no real guest processes, a spin-without-syscalls scenario cannot be detected or killed.

- Spec: `specs/baud-multiverse.md:112-114` (§6 Multi-Guest Clusters)
- Repro: `grep -n "watchdog\|SIGALRM\|wall.clock\|quantum" crates/baud-multiverse/src/lib.rs` — no results.
- Scope: implement a wall-clock watchdog thread (or `SIGALRM`-based timer) that detects when a guest has not yielded at a syscall boundary within a configured quantum; kill the offending guest with a `Crash{detail: "quantum-overrun"}` report; document this as outside the deterministic boundary.

#### VR2-M8: CLI command surface uses 'baud keys' instead of spec-defined 'baud secrets'

The spec (`specs/baud-cli.md:54`) defines `baud secrets init|edit|show --redacted|rotate` as the command surface. The implementation registers the subcommand as `baud keys` (`crates/baud-cli/src/main.rs:39`, `cmds/keys.rs`). Running `baud secrets` fails with `error: unrecognized subcommand`. The drive scripts use `baud keys show` (in `drive/m0.sh`) which passes, but the spec-defined command name is absent.

- Spec: `specs/baud-cli.md:54`
- Repro: `baud secrets init` → `error: unrecognized subcommand 'secrets'`.
- Scope: rename the CLI subcommand from `keys` to `secrets`; update all drive scripts and documentation referencing `baud keys` to use `baud secrets`; keep `baud keys` as a deprecated alias or remove it after updating all references.

#### VR2-M9: GET /tapes/{id} returns 200+deleted for killed tapes; spec requires failure

After `baud tape kill <id>` (`DELETE /tapes/{id}`), the server sets `state='deleted'` in SQLite but keeps the row. A subsequent `GET /tapes/{id}` returns HTTP 200 with `state='deleted'`. The `Backend` trait spec and its conformance test (`baud-tape/src/backend.rs:151-152`) explicitly require: 'status after delete must fail (sandbox gone)'. The server-layer route `/tapes/{id}` violates this invariant by returning 200.

- Spec: `specs/baud-tape.md:62-63`; `crates/baud-tape/src/backend.rs:151`
- Repro: create tape → kill → `GET /tapes/{id}` returns HTTP 200 with `state='deleted'`.
- Scope: change `GET /tapes/{id}` to return HTTP 404 (or 410 Gone) when the tape row has `state='deleted'`; update the CLI handler to treat this as a not-found error; add or update the conformance test to cover this case.

#### VR2-M10: --json flag does not produce machine-readable output on errors

Spec (`specs/baud-cli.md:80-83`) requires `--json` on every command with machine-readable output. When any HTTP call fails (server down, 404, etc.), the CLI emits a multi-line human-readable error to stderr — not a JSON object. Stdout is empty, exit code is 1. Scripts that parse `--json` output cannot distinguish or parse error cases.

- Spec: `specs/baud-cli.md:80-83`
- Repro: `baud tape ls --json` (server down) → stderr: `Error: GET http://127.0.0.1:7734/tapes: could not connect to baud-server\nCaused by:\n  0: ...`; stdout empty.
- Scope: when `--json` is set and an error occurs, emit `{"ok": false, "error": "<message>"}` to stdout (not stderr) and exit 1; cover at minimum: connection refused, 404, and 5xx responses.

#### VR2-M11: Exit code 2 not implemented for baud run status; response missing exit_code field

Spec (`specs/baud-cli.md:80-83`, `:92-94`) shows `baud run status` returning a JSON object with an `exit_code` field, and the CLI exiting with code 2 on goal/violation. The run status JSON response has no `exit_code` field — it has a `status` field with values like `running`/`aborted` (`crates/baud-server/src/routes/runs.rs`). The CLI `baud run status` handler never calls `process::exit(2)` for any outcome. Only `baud run fuzz` (`cmds/fuzz.rs:74`) emits exit 2.

- Spec: `specs/baud-cli.md:80-83`; `specs/baud-cli.md:92-94`
- Repro: run a spec to completion with a goal probe; `baud run status <id> --json` returns `{"status": "done"}` with no `exit_code` field; process exits 0.
- Scope: add `exit_code` field to the run status response (0 = completed, 1 = error/aborted, 2 = goal/violation); implement `process::exit(2)` in the `baud run status` CLI handler when `exit_code == 2`.

#### VR2-M12: same_seed_same_replies_same_tape test missing; replacement test is weaker

Spec (`specs/baud-driver.md:109-115`) mandates a test named `same_seed_same_replies_same_tape` that calls `run_driver(seed, &script)` twice and asserts `a.tape_bytes() == b.tape_bytes()` — two independent full driver runs with the same seed and same observation script produce identical tapes. The implementation at `crates/baud-driver/src/lib.rs:612` has `determinism_property_same_seed_same_draws`, which records a live run's tape then replays it through a `ReplayEngine` — a single run replayed, not two independent runs. The spec's `run_driver(seed, script)` helper and `tape_bytes()` method do not exist.

- Spec: `specs/baud-driver.md:109-115`
- Repro: `grep -n "same_seed_same_replies_same_tape" crates/baud-driver/src/lib.rs` — no results.
- Scope: add a `run_driver(seed: u64, script: &[Observation]) -> Tape` helper; add `tape_bytes()` to the tape type; add the `same_seed_same_replies_same_tape` test that calls `run_driver` twice with identical arguments and asserts byte-identical output.

#### VR2-M13: Divergence reporting omits node/probe/syscall identity of first mismatching step

Spec (`specs/baud-journal.md:89-91`) states: 'the first step whose observation hash differs from the journal is reported, naming the node/probe/syscall that diverged'. The poisoned verify endpoint at `crates/baud-server/src/routes/verify.rs:257` emits only `first_divergent_step: <step_number>`. The JSON response has no `divergent_node`, `divergent_probe`, or `divergent_syscall` fields.

- Spec: `specs/baud-journal.md:89-91`
- Repro: call `POST /verify/determinism/poisoned` — response contains only `first_divergent_step: 0` with no node/probe/syscall.
- Scope: when a divergent step is found, look up the corresponding observation record and include `divergent_node`, `divergent_probe`, and `divergent_syscall` in the response JSON; update the CLI formatter to display these fields.

#### VR2-M14: verify determinism returns ok=true with zero observations when multiverse load fails

When `Multiverse::load` fails, `run_spec_through_multiverse` at `crates/baud-server/src/routes/verify.rs:417-421` logs a warning and returns `Vec::new()`. Both runs produce identical empty observation streams, causing `verify determinism` to report `ok: true, observation_count: 0` — a false positive. Spec (`todo.md:75`) states 'The claim is verified, not assumed'; returning verified=true on an empty stream is unverified.

- Spec: `todo.md:75`
- Repro: `POST /verify/determinism` with a spec whose binary is absent → response: `{"ok": true, "observation_count": 0}`.
- Scope: treat zero observations as a verification failure; return `{"ok": false, "error": "multiverse load failed — no observations produced"}` when either run produces an empty stream.

#### VR2-M15: Divergent runs not marked or excluded from replay/shrink/reconstruct

Spec (`specs/baud-journal.md:91`) states: 'A divergent run is marked and excluded from replay/shrink/reconstruct.' The runs table schema (`crates/baud-server/migrations/0003_runs.sql:34`) enumerates allowed statuses as `pending | provisioning | running | done | failed | aborted` — no `divergent` status. The replay route at `crates/baud-server/src/routes/replay.rs:34` queries runs by id without checking for a divergent status. No code path marks a run as divergent or rejects it from replay.

- Spec: `specs/baud-journal.md:91`
- Repro: `grep -rn "divergent" crates/baud-server/src/` — no status value or guard exists.
- Scope: add `divergent` to the allowed run statuses in the migration and the server; mark a run as divergent when `verify determinism` reports `ok: false`; add guards in replay, shrink, and reconstruct routes to return 409 Conflict for divergent runs.

#### VR2-M16: planted_bug_needs_the_interleaving test absent; replacement makes no assertions

Spec §6 (`specs/baud-raftlet.md:88-97`) shows a test `fn planted_bug_needs_the_interleaving` with two assertions: (1) `run(random_drops(), budget).outcome.is_none()` — random drops never find the bug; (2) `run(guided(), budget).outcome` matches `Some(Crash { invariant: Some(i), .. }) if i == "log_prefix_agreement"` — the guided run finds it. The actual test in `crates/baud-raftlet/src/lib.rs:779` is named `simulate_with_bug_can_be_driven_to_violation` and contains `let _ = found_violation;` with an explicit comment 'We don't assert found_violation here'. Neither spec assertion is present; the test only verifies the code compiles.

- Spec: `specs/baud-raftlet.md:88-97` (Testing §6 code block); `crates/baud-raftlet/src/lib.rs:779-819`
- Repro: `grep -n "planted_bug_needs_the_interleaving" crates/baud-raftlet/src/lib.rs` — no results.
- Scope: add `fn planted_bug_needs_the_interleaving` with both assertions; implement `run(tactics, budget)` helper returning an `Outcome?`; ensure the random-drops run returns `None` and the guided run returns `Some(Crash { invariant: "log_prefix_agreement" })`.

#### VR2-M17: /runs/raftlet/fuzz endpoint missing; M7 drive references non-existent route

The M7 drive script (`drive/m7.sh:74`) calls `POST /runs/raftlet/fuzz` with `tactics: markov-crash-restart` and `planted_bug: true`. This route is not registered in `crates/baud-server/src/main.rs` (the router only has `/runs/fuzz`). Calling it returns an axum 404, causing JSON parse failure in the drive script. Spec §5 (`specs/baud-raftlet.md:58`) states tactics are `markov-partition` + `crash-restart` but these are not accepted by any raftlet-specific server endpoint.

- Spec: `specs/baud-raftlet.md:58` (tactics: markov-partition + crash-restart); `drive/m7.sh:74`; `crates/baud-server/src/main.rs:85`
- Repro: `curl -X POST http://localhost:7734/runs/raftlet/fuzz` → axum 404.
- Scope: register `POST /runs/raftlet/fuzz` in the server router; implement it to accept `tactics: markov-partition | markov-crash-restart` and `planted_bug: bool`, dispatching to `baud_raftlet::simulate()`; update `drive/m7.sh` to use the correct endpoint.

#### VR2-M18: baud-tracing has no aya dependency — Native eBPF path is structurally incomplete

Spec (`specs/baud-tracing.md:39-48`, `todo.md:131`) requires an aya-based CO-RE probe set loading prebuilt BPF objects (ringbuf → agent → server) as the primary native path. The crate comment at `crates/baud-tracing/src/lib.rs:23` explicitly states 'NO aya in this crate'. No `aya` dependency appears in `Cargo.toml`. No prebuilt BPF object files exist anywhere in the tree. `BpfAvailability::Native` can be returned on Linux when `BPF_PROG_LOAD` succeeds, but there is no code to load or attach CO-RE probes after detecting Native availability — the `ingest_syscall`/`ingest_sched_switch` functions are always used regardless of mode, so the 'independent witness' property is never achieved.

- Spec: `specs/baud-tracing.md:39-48` (§2 Crate Architecture 'aya CO-RE probes (prebuilt) → ringbuf'); `todo.md:131`
- Repro: `grep "aya" crates/baud-tracing/Cargo.toml` — no results; `find . -name "*.bpf.o" -o -name "*.bpf.c"` — no results.
- Scope: add `aya` and `aya-obj` as dependencies; provide or reference prebuilt BPF object files for sched/exec/syscall/fault probes; implement the Native path to load and attach these probes and drain the ringbuf; keep Fallback path for denied environments.

#### VR2-M19: M6.5 probe violation_found=1.0 never emitted; fuzz route records wrong probes

The M6 drive spec comment documents check M6.5 as 'crashed run observations stored (violation_found=1.0 present)'. However, the fuzz route only records `depth` and `crashed` probes (from the parser simulation), never `violation_found`. A direct query `baud obs ls --run <id> --json` returns only probes named 'depth' and 'crashed'. The `violation_found=1.0` observation required by the spec is never emitted.

- Spec: `specs/baud-raftlet.md:75` ('A violated invariant emits `Crash{invariant: log_prefix_agreement}`'); `drive/m6.sh:10` comment
- Repro: `baud obs ls --run <raftlet-run-id> --json | jq '.[] | .probe'` — only `depth` and `crashed` appear.
- Scope: when `baud_raftlet::simulate()` finds the `log_prefix_agreement` violation, emit an `Observe{probe: "violation_found", value: 1.0}` record; update the M6 drive check M6.5 to assert this probe is present.

#### VR2-M20: baud-stream render does not materialize pixel bytes — storage-discipline replay unimplemented

Spec §5 (`specs/baud-stream.md:80-84`) states: 'Pixels are regenerated on demand: stream render replays the tape prefix under the supervisor with capture enabled and materializes the frames.' `crates/baud-server/src/routes/stream.rs:135-194` `render()` queries stored frame metadata and returns a JSON summary with the caller-supplied output path echoed back, but writes no Y4M or QOI bytes to disk. The CLI at `crates/baud-cli/src/cmds/stream.rs:103` explicitly acknowledges: '(would write Y4M to {o} — replay not yet implemented)'. The M5 drive step passes without verifying the output file was created.

- Spec: `specs/baud-stream.md:80-84` (§5 Storage Discipline); `specs/baud-stream.md:99` (`stream render --run … -o PATH`); `todo.md:194` (M5 drive: 'stream render … materializes the gradient sequence and a re-render is byte-identical')
- Repro: `baud stream render --run <id> --format y4m -o /tmp/out.y4m` → command exits 0, but `/tmp/out.y4m` does not exist.
- Scope: implement `stream render` to replay the tape prefix through the supervisor with frame capture enabled, collect `FrameRecord`s, encode them as Y4M (or QOI), and write the bytes to the `-o PATH` file; update the M5 drive to assert the output file exists and has non-zero size.

#### VR2-M21: drive/m7.sh omits deliberately-broken-supervisor negative test

`todo.md:198` specifies the M7 drive must include: 'a deliberately broken supervisor build (test fixture) fails the cross-check'. `drive/m7.sh` has eight checks (M7.1–M7.8) that all exercise healthy-run paths and `source=fallback` visibility, but contains no step that introduces a supervisor bug and asserts the cross-check returns `passed=false`. No test fixture that produces a deliberately wrong syscall log exists anywhere in the codebase.

- Spec: `todo.md:198` (M7 drive: 'deliberately broken supervisor build (test fixture) fails the cross-check'); `specs/baud-tracing.md:91-101` (§6 Testing)
- Repro: `grep -n "broken\|fixture\|false" drive/m7.sh` — no negative-case step.
- Scope: create a test fixture (e.g., a patched supervisor build or a mock that emits one extra syscall) that produces a deliberately wrong plane-1 log; add M7.9 to `drive/m7.sh` asserting that `baud verify observation` returns `passed=false` against this fixture.

---

### MINOR

#### VR2-m1: baud-proto uses thiserror dependency beyond spec-declared {serde, ciborium}-only constraint

Spec (`specs/baud-proto.md:48`) states: 'Deps = {serde, ciborium} only; no tokio, no chrono'. The `Cargo.toml` at `crates/baud-proto/Cargo.toml` lists `thiserror` as a production dependency, used for `EncodeError` and `DecodeError` types at `crates/baud-proto/src/lib.rs:466-478`. This violates the deps-only constraint.

- Spec: `specs/baud-proto.md:48`
- Repro: `grep "thiserror" crates/baud-proto/Cargo.toml` — present as a non-dev dependency.
- Scope: replace `thiserror`-derived error types with manual `impl std::error::Error` and `impl Display`; remove `thiserror` from `crates/baud-proto/Cargo.toml`.

#### VR2-m2: der_from_ed25519_spki is dead code in baud-identity

The function `der_from_ed25519_spki` at `crates/baud-identity/src/lib.rs:237-247` is defined but never called. The compiler emits a warning: 'function `der_from_ed25519_spki` is never used'. The verification path in `verify()` at line 167-172 uses `DecodingKey::from_ed_der(&pk_bytes)` with the raw 32-byte public key, bypassing this helper entirely. Dead cryptographic encoding code creates maintenance confusion and may indicate the verification path is not using the correct DER-SPKI format.

- Spec: `specs/baud-identity.md:27-28`
- Repro: `cargo build -p baud-identity 2>&1 | grep "never used"` — warning about `der_from_ed25519_spki`.
- Scope: investigate whether `verify()` should use the DER-SPKI helper instead of raw bytes; if the SPKI format is correct per the spec, wire `der_from_ed25519_spki` into the verification path and add a test; if raw bytes are correct, remove the dead function.

#### VR2-m3: run() returns Result<ObservationStream, E> instead of spec's infallible ObservationStream

Spec §5 (`specs/baud-multiverse.md:101`) declares `fn run(&mut self, tape: impl DrawSource) -> ObservationStream` (no `Result`). Implementation at `crates/baud-multiverse/src/lib.rs:447` returns `Result<ObservationStream, MultiverseError>`. The spec test at §8 (`let obs1 = m1.run(tape.clone()).hash_stream()`) treats the return as directly usable without unwrapping. This changes the call-site contract for all consumers.

- Spec: `specs/baud-multiverse.md:101`
- Repro: `grep -n "fn run" crates/baud-multiverse/src/lib.rs` — returns `Result<ObservationStream, MultiverseError>`.
- Scope: change `run()` to return `ObservationStream` directly, encoding any launch errors as a terminal `Crash` observation in the stream rather than a `Result`; update all call sites.

#### VR2-m4: dedup_by_plaintext_hash test absent — wrong name used in implementation

Spec (`specs/baud-journal.md:106-110`) defines a required test named `dedup_by_plaintext_hash` that appends identical plaintext twice and asserts the resulting `address` values are equal. The implementation at `crates/baud-journal/src/lib.rs:409` has `content_addressing_deduplication`, which tests the same property but under a different name. The spec-mandated name is the normative reference.

- Spec: `specs/baud-journal.md:106-110`
- Repro: `grep -n "dedup_by_plaintext_hash" crates/baud-journal/src/lib.rs` — no results.
- Scope: rename `content_addressing_deduplication` to `dedup_by_plaintext_hash` (or add an alias) to match the spec-mandated test name exactly.

#### VR2-m5: doctor local_backend_vm_ok is always null — lima/colima VM not checked

Spec (`specs/baud-tape-local.md:47-49`) states: 'On macOS dev machines, runs inside a lima/colima VM (checked by doctor), since the supervisor needs Linux.' The doctor route (`crates/baud-server/src/routes/doctor.rs:22`) hard-codes `"local_backend_vm_ok": null` as a stub. `LocalBackend::new()` calls `detect_lima()` which only checks if `limactl` is on PATH; if absent, exec runs on the host kernel without a VM. This is not surfaced as a doctor failure.

- Spec: `specs/baud-tape-local.md:47-49`; `crates/baud-server/src/routes/doctor.rs:22`
- Repro: `baud doctor --json | jq .local_backend_vm_ok` → `null`.
- Scope: implement `local_backend_vm_ok` in the doctor route to check whether `limactl` is installed and a baud VM is running; return `false` (not `null`) when the VM is absent on macOS; surface this as a warning in `baud doctor` human output.

#### VR2-m6: Missing enforces_sandbox_shape test for DaytonaBackend

Spec §6 (`specs/baud-tape.md:93-96`) defines a mandatory test: `fn enforces_sandbox_shape() { let s = client.build_spec(); assert_eq!((s.vcpu, s.ram_gib, s.autostop_s), (1, 1, 60)); }`. This test is absent from `crates/baud-tape/src/daytona.rs`. The `DaytonaBackend` passes the caller's `SandboxSpec` values to the API without enforcing the hard constraints (1 vCPU / 1 GiB / 1 GiB / auto-stop 60s).

- Spec: `specs/baud-tape.md:93-96`
- Repro: `grep -n "enforces_sandbox_shape" crates/baud-tape/src/` — no results.
- Scope: add `enforces_sandbox_shape` test to `crates/baud-tape/src/daytona.rs`; enforce the hard constraints in `DaytonaBackend::create()` (clamp or override caller-supplied values before sending to the API).

#### VR2-m7: No recorded-fixture contract tests for Daytona backend

Spec (`specs/baud-tape.md:92`) says 'contract tests replay recorded request/response fixtures — no live API in CI'. The implementation has only a comment 'Recorded-fixture contract tests (not run in CI without a real API key)' at the top of `daytona.rs` but zero actual fixture-based test functions. The only tests in `baud-tape` target a `StubBackend`. API drift cannot be caught without the fixtures.

- Spec: `specs/baud-tape.md:92`; `crates/baud-tape/src/daytona.rs:7`
- Repro: `grep -n "#\[test\]" crates/baud-tape/src/daytona.rs` — no test functions.
- Scope: record a minimal set of HTTP request/response fixtures for create/status/exec/delete API calls; add tests that replay these fixtures using a mock HTTP client; run these tests in CI without requiring a live API key.

#### VR2-m8: baud server logs --follow is silently ignored

Spec (`specs/baud-cli.md:55`) defines `baud server logs [--follow]`. The `--follow` flag is accepted by the CLI parser but the handler does `follow: _` (unused, `crates/baud-cli/src/cmds/server.rs:137`) and unconditionally returns a stub response `{"logs": [], "note": "streaming logs not yet implemented"}`. There is no SSE or polling fallback. The omission is not documented in the CLI help text.

- Spec: `specs/baud-cli.md:55`; `crates/baud-cli/src/cmds/server.rs:137`
- Repro: `baud server logs --follow` — returns immediately with empty logs stub; does not stream.
- Scope: implement `--follow` as SSE streaming from the server's log endpoint, or at minimum a polling loop that fetches new log lines; update the CLI help text to reflect the current implementation status if full streaming is deferred.

#### VR2-m9: infra/pkgs/ absent — cross-build infrastructure for static musl agent missing

Spec (`specs/baud-tape-agent.md §2`) states the binary is 'Cross-built (macOS host → static musl x86_64 linux) by the `infra/pkgs` fenix overlay (plan §11.2)'. `infra/` contains only `infra/secrets/`; `infra/pkgs/` does not exist. The built `baud-agent` binary is a macOS arm64 Mach-O executable, not a static musl x86_64 Linux binary. Spec mandates binary size ≤ 10 MiB — there is no CI check enforcing this.

- Spec: `specs/baud-tape-agent.md §2` (Crate Architecture, Rationale); `todo.md:311`
- Repro: `ls infra/` — only `secrets/` present; `file target/debug/baud-agent` — Mach-O executable.
- Scope: create `infra/pkgs/` with the fenix overlay (`default.nix`, `baud-agent.nix`, `baud-multiverse.nix`) targeting static musl x86_64-linux; add a CI check asserting the resulting binary is `<= 10 MiB`; update `doctor` to validate the cross toolchain is available.

#### VR2-m10: baud-packages spec test guest_is_static_no_pie not implemented against real ELF

Spec §5 (`specs/baud-packages.md §5`) defines the normative test `fn guest_is_static_no_pie() { let elf = build(spec).guest; assert!(elf.is_static() && !elf.is_pie()); }`. The test suite in `crates/baud-packages/src/lib.rs` contains `stub_build_contract_check_skipped` which explicitly skips the contract check for stubs, but there is no test that exercises `BuildResult::verify_guest_contract()` on a real ELF output.

- Spec: `specs/baud-packages.md §5` (Testing)
- Repro: `grep -n "guest_is_static_no_pie" crates/baud-packages/src/lib.rs` — no results.
- Scope: add `fn guest_is_static_no_pie()` test that calls `build()` with the hello-deterministic spec (or a minimal test spec) and asserts `verify_guest_contract()` passes; gate this test behind a `#[cfg(feature = "integration")]` flag or `#[ignore]` so it skips in CI without nix.

#### VR2-m11: baud-init fixture path escape security control not implemented

Spec §8 (`specs/baud-init.md §8`) Security says 'Fixtures written only under the sandbox workdir'. `crates/baud-init/src/parse.rs:94-104` accepts any string as `FilesEntry.path` with no validation — a path like `../../etc/passwd` or `/etc/passwd` is accepted without error by `baud_init::lint()`. The closed directive schema does not enforce the path-containment invariant.

- Spec: `specs/baud-init.md §8` (Security Considerations — Fixture path escape)
- Repro: create a spec with `files: [{path: "../../etc/passwd", content: "..."}]`; `baud spec lint` reports success.
- Scope: add path validation in `parse.rs` (or in `lint()`) that rejects absolute paths and any path containing `..` components; return a lint error with a descriptive message; add a test asserting both path forms are rejected.
