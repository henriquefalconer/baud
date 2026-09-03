HARD STOP: one iteration = one submittal package, taken from @todo-submittal.md. A package is complete when its evidence is produced, reviewed, stamped with a review action, and the register entry is written and pushed to `main`. When the package is closed, append the final progress block, emit a single promise (`<promise>NEXT</promise>` if @todo-submittal.md still has pending items, `<promise>COMPLETE</promise>` if it has zero), and stop.

**Selection lock.** At the beginning of the main session, record the exact pending package or entries selected from `@todo-submittal.md`. They are this iteration's contract. Before the final progress block, commit, or promise, re-read `@todo-submittal.md` and check every entry you selected. Every selected entry must be completely addressed and collapsed to its own `DONE` line. A partial evidence pass or a reviewed subset does not close the package while a selected entry remains pending. Continue the same session until all selected entries are complete, validated by the existing procedure, and updated to `DONE` in `@todo-submittal.md`; only then append the final block, commit, push, and return a promise tag.

**One package per iteration.** A submittal is evidence about the business, not a survey of the repository — do not batch two domains into one package to look faster.

## 0. Study before you touch anything

0a. Read this whole file. Sections 1–5 below are the standard: what a package contains, how it is reviewed, how substitutions and open questions are filed, the corpus facts that bind every submittal, and what not to do. Sections 6–7 carry the queue and the blocked domains.
0b. Read `@todo-submittal.md` — the register: the decisions already accepted, and under **Open**, the product decisions still to be made, grouped by submittal and carrying a stable `OQ-nn-mm` id. The accepted half **is binding**: a package may not contradict an entry, and where an entry covers the domain, the package cites it rather than re-deciding it. The open half is the opposite — a package may not answer an open question in passing; it cites the id and leaves it open. Open questions are never "pending items" for the promise: a pending item is a submittal package still to be produced.
0c. Read `AGENTS.md`, and the `specs/*.md` the item names. Use parallel subagents for the reading and the searches.
0d. **Evidence about the business is the deliverable. The state of the repository is not evidence.** The corpus tells you what the business does, which values exist and how people work. The schema, the bindings, the routes and the missing tables tell you only what has been coded so far, and that never narrows what should be built. "There is no column for this" is an implementation note, never a reason to shrink a goal.

## Progress Logging — Mandatory

`ralph/progress.txt` has two jobs: it is the watchdog's only liveness signal, and it is the user's live view of what you are doing (the file is tailed in their terminal). Append with `printf '\n%s\n' "<one-liner>" >> ralph/progress.txt` so each entry sits on its own blank-led line.

The first thing you do is append (the iteration number is exactly "[ralph-iteration]"):
```
═══════════════════════════════════════════════════════
  Ralph Iteration [ralph-iteration] (Submittal Mode)
═══════════════════════════════════════════════════════

Brief explanation of what you will do (starting with a verb like "Reading todo-submittal.md to pick the next pending package...", ending in ...)

```
The first line appended must be exactly "═══════════════════════════════════════════════════════".

Then narrate as you go — the corpus queries you ran and what they returned, each substitution you propose, each question you file. Lean toward narrating more rather than less; silence looks like a stall.

After re-reading `@todo-submittal.md` and confirming every item selected at the beginning of this main session is now a `DONE` entry, append the block BELOW FIRST, THEN commit so it is part of the same commit:
```

## <the current UTC time, resolved — e.g. 2026-08-19T03:48:01> UTC - Submittal committed.
- The package and its review action
- What the corpus actually showed, with counts
- **Substitutions and open questions:**
  - [item 1]
  - [item 2]
---
```
Resolve the timestamp yourself — run `date -u +%Y-%m-%dT%H:%M:%S` and paste the result. Do NOT append the literal `$(date ...)`: this block is written through Write/Edit, where no shell expansion happens, so a command substitution lands in the log verbatim.

## Stop Condition

Only after the register entry is written, every entry selected at the beginning of this main session is confirmed `DONE` in `@todo-submittal.md`, and the commit is pushed, reply with one of:

- `<promise>NEXT</promise>` if @todo-submittal.md still has pending items, or if the package could not be closed.
- `<promise>COMPLETE</promise>` if @todo-submittal.md has zero pending items (verify by re-reading).

Do not perform any additional work after the promise.

## You are operating autonomously

Nothing will resume you: this session ends when you stop, and an unfinished package is lost.

Long commands: one Bash call per step, each with `timeout` (max 600000 ms), and a progress note before each — never chain with `&&`, and never background a command whose result you need, since a backgrounded command dies with the session. To wait, poll inside ONE call (`until <check>; do sleep 5; done`) or use `Monitor`. Never use `ScheduleWakeup`: its delay is clamped to 60s while this session exits in about ten, so the wakeup can never fire, yet it reports one as scheduled.

Before ending, re-read your last paragraph. If it is a plan, a question, or a promise about work you have not done ("Waiting for…", "I'll…"), the package is not closed — do it now.

**Never `git add -A`.** Commit only the paths this package changed. A concurrent build loop owns `packages/`, `todo-build.md` and `ralph/progress.txt`; its dirty tree is expected and is not yours to clean.

---

# prompt-submittal.md — the evidence a domain must produce before it is built

**For a fresh session.**

**Evidence about the business is the deliverable. The state of the repository is not evidence.**
The corpus tells you what the business does, which values exist and how people work — that is the
point of this track. The schema, the bindings, the routes and the missing tables tell you only
what has been coded so far, and **that never narrows what should be built**. "There is no column
for this" is an implementation note, never a reason to shrink a goal. State the goal, design it
whole, and let the build follow.

**Existing code is never a constraint.** A submittal defines what gets built and may replace any
feature from scratch; the product is remodelled, recoded and redone to match what the submittal
established, never the reverse.

---

## 1 · What a package contains

1. **The spec.** File(s) under `specs/`, structured so a brief reads the catalogues directly
   instead of re-deriving them from prose.
2. **Cross-references.** Spec section, `file:line` citations, source URLs, and the `SC-nn` this
   unblocks.
3. **Supporting documentation.** Files, sizes, row counts, dates. The provenance of the map
   lives inside the map.

---

## 2 · Review actions

The same five words adjudicate a submittal here and a plan export in `todo-plan.md`.

| Action | Effect |
|---|---|
| **No Exceptions Taken** | Proceed. |
| **Make Corrections Noted** | Proceed, incorporating the markups. No resubmittal. |
| **Revise and Resubmit** | Do not proceed. Correct and send again. |
| **Rejected** | Non-compliant. Furnish what was specified. |
| **Reviewed for Record** | Informational. No action. |

---

Add every validated item as detailed bullet points on `todo-plan.md`, including Purpose and Standing tasks, with the lines of each spec as reference.
