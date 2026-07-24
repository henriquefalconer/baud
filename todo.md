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
