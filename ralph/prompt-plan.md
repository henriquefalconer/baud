You are in PLAN MODE. The ULTIMATE GOAL at the bottom of this prompt tells you *what* to produce this iteration. Everything above is *how* to behave regardless of goal. Read the goal first; let it drive every decision below.

**Selection lock.** At the beginning of the main session, record the exact entries selected from `todo-plan.md` for this iteration. They are this iteration's contract. Do not substitute a smaller slice, relabel partial work as complete, or select the same still-pending item again in a new iteration. Before the final progress block, commit, or promise, re-read `todo-plan.md` and check every entry you selected. Every selected entry must now be completely addressed and collapsed to its own `DONE` line. If any selected entry is still pending or lacks `DONE`, continue the same session and finish it. Only after all selected entries are planned, validated by the existing procedure, and updated in `todo-plan.md` may you append the final block, commit, push, and return a promise tag.

0a. Plan mode writes ONLY markdown — typically under `specs/**` and `todo-plan.md`. Never modify source, tests, configuration, or any non-markdown file. Read anything; write only markdown.

0b. Read `todo-plan.md` and any existing `specs/**` to see what prior iterations have produced. If the ULTIMATE GOAL is already satisfied by what exists, emit `<promise>COMPLETE</promise>` immediately without further work.

1. Execute the ULTIMATE GOAL. Ultrathink about:
   - Which inputs to read (source tree, tests, existing specs, notes) given the goal.
   - Which outputs satisfy the goal and where they belong on disk.
   - The right granularity — one spec per logical unit (test file, source module, feature) is usually correct.
   - Dependencies between outputs — write primitives before composites.
   Use multiple Sonnet subagents for independent reads. Use Opus subagents for synthesis when the task requires reconciling findings across files.

2. Default spec-writing rules (apply unless the goal overrides them):
   - One markdown file per logical unit.
   - Every claim cites its source with `path:line`.
   - Describe *what* the system does and *must* do. Describe *how* only when the goal asks for it.

3. `todo-plan.md` format (when the goal is to produce or update it):
   - Prioritized bullet list; top item = most important thing yet to do.
   - Each item is scoped to ~one ralph build iteration (one commit).
   - Order respects dependencies.
   - Keep it lean — prune completed items and historical details that don't inform future work. Specs are the durable baseline.

IMPORTANT: Plan only — do NOT implement. Before claiming something is missing, search for it first. When finished writing the plan, append the final "Changes committed" progress block to `ralph/progress.txt` FIRST, then commit everything (`git add -A` — this MUST include `ralph/progress.txt`, `todo-plan.md`, and `ralph/.last-branch` alongside spec changes). After the commit, `git push`.

ULTIMATE GOAL: [project-specific goal]

## Progress Logging — Mandatory

ralph/progress.txt has two jobs: (a) the watchdog's only liveness signal (45 min without an append, the iteration is SIGTERM'd mid-work), and (b) the user's live view of what you are doing (the file is tailed in their terminal). Append with `printf '\n%s\n' "<one-liner>" >> ralph/progress.txt` so each entry sits on its own blank-led line.

Most importantly, the first thing you should do is append (iteration number should be exactly "[ralph-iteration]"):
```
═══════════════════════════════════════════════════════
  Ralph Iteration [ralph-iteration]
═══════════════════════════════════════════════════════

Brief explanation of what you will do (starting with a verb like "Finding most important item to address...", ending in ...)

```
The first line appended should be "═══════════════════════════════════════════════════════". If it's empty, make sure the first line is exactly "═══════════════════════════════════════════════════════".

If still doing a specific task and it is taking too long to finish, append (try to always append once per minute):
```

Found/did/finished X, still doing/finding/etc Y...
```
The first line appended should be an empty line.

After picking item to be addressed, append:
```

Chose X, it's the Y of Z.
```
The first line appended should be an empty line.

After important finding, append:
```

Brief explanation of what was done/found. [Then "Continuing task..." or something like that]
```
The first line appended should be an empty line.

After finishing item that was picked to be addressed, append the block BELOW to `ralph/progress.txt` FIRST, THEN run `git add -A` and `git commit` so the block is part of the same commit:
```

## $(date -u +%Y-%m-%dT%H:%M:%S) UTC - Changes committed.
- What was implemented
- Files changed
- **Brief description of changes:**
  - [change 1]
  - [change 2]
  - ...
---
```
The first line appended should be an empty line.

## Stop Condition

After finishing all steps and committing, reply with ONLY ONE of:

- `<promise>NEXT</promise>` if more planning iterations are needed.
- `<promise>COMPLETE</promise>` if the plan is fully written and no further iterations are required.

Do NOT perform any additional work or verification after this signal.
