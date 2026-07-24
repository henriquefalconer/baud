<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud CLI Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-23

---

## 1. Overview

### Purpose

`baud-cli` is the single `baud` binary and the only interface to the system. Every capability of every
component is reachable and testable through it. It is a thin client: one subcommand maps to one server call
plus one formatter.

### Goals

- **Completeness**: nothing is reachable only by editing files or calling internals
- **Scriptability**: `--json` on every command; exit codes are stable
- **Thinness**: zero business logic; no local state beyond server address and auth
- **No workload sugar**: workloads are addressed via `--spec`, never a `baud mario`-style subcommand

### Non-Goals

- Doing work the server should do
- Interactive TUI (drive scripts and `--json` are the interface)

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│                  baud-cli                    │
│  clap subcommands → HTTP → baud-server       │
│  human table  |  --json passthrough            │
└──────────────────────────────────────────────┘
```

### Rationale

- Each subcommand = one server call + one formatter. Business logic lives server-side.

---

## 3. Command Surface

```
baud server   start|stop|status|logs [--follow]
baud doctor
baud secrets  init|edit|show --redacted|rotate
baud spec     new|lint|show <spec.toml>
baud tape     create|ls|status|ensure|kill|reconstruct|exec|probe-caps <id>
baud run      start --spec S --strategy ST --tactics T --seed N --budget-minutes M
baud run      ls|status|watch|pause|resume|abort <run>
baud obs      ls|get|tail --run <id> [--probe X] [--node I]
baud syscalls tail|get --run <id> [--node I] [--sysno N]
baud tracing     tail --tape <id> [--event ...] [--node I] ; summary --run <id>
baud net      weather --run <id>
baud stream   tail --run <id> [-o out.y4m] [--hashes-only]
baud stream   render --run <id> [--from-step A --to-step B] [--format qoi-seq|y4m] -o PATH
baud stream   frames --run <id> [--node I]
baud verify   determinism --spec S --seed N [--times 2]
baud verify   observation --run <id>
baud shrink   <run> [--passes chunk-delete,zero,hold-shorten]
baud replay   <run> [--tape-file F] [--to-step K]
baud budget
```

---

## 4. Global Conventions

| Convention | Rule |
| ---------------- | ------------------------------------------ |
| `--json`         | Available on every command; machine-readable output |
| Exit codes       | `0` completed · `1` error · `2` goal/violation |
| Server address   | From `BAUD_SERVER` env or default localhost port |
| Auth             | Local token; never printed |

---

## 5. Testing

```bash
# drive-script assertion style
out=$(baud run status "$id" --json)
[ "$(jq -r .exit_code <<<"$out")" = "2" ]                     # goal/violation
baud obs get --run "$id" --probe secret --json | grep -q '\[REDACTED\]'
```

- Every `drive/*.sh` milestone script exercises the relevant subcommands and asserts on `--json` output.

---

## 6. Security Considerations

| Threat                          | Handling                                    |
| ------------------------------- | ------------------------------------------- |
| Token in argv/env               | Auth read from a file/agent, never a flag; never printed |
| Accidental secret echo          | `--json` redacts `SecretString` fields as `[REDACTED]` |
| Destructive commands            | `tape kill` / `run abort` require an explicit id; no wildcards |

---

## 7. Future Considerations

| Feature       | Description                              |
| ------------- | ---------------------------------------- |
| Shell completion | Generated clap completions            |
| Watch dashboards | Richer `run watch` rendering          |
