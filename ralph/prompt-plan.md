HARD STOP: one planning iteration is one full pass of the procedure in §1 over every standing task group in
`todo-plan.md`. A pass is complete when every group has been audited against the actual implementation, the
main thread has merged complete actionable tasks into `todo-build.md` as bullet points, the plan and queue
agree, required markdown changes are written, and **absolutely everything** is committed and pushed to
`main`. Then append the final progress block, emit one `<promise>COMPLETE</promise>` promise, and stop.

The orchestrator schedules later Ralph runs. Finish this pass after the push.

## 0. What this loop is

`<promise>COMPLETE</promise>` from the planning loop is a claim about this pass, not a claim that Baud is
built. It means every standing group was examined, the queue was reconciled, and the planning changes were
committed and pushed. It does not mean the implementation queue is empty or that every standing group is
finished. Later passes continue from the current files.

0a. Read **this whole file**. Everything below the divider is the procedure in full. The rules are mandatory.
0b. Read `todo-plan.md` and `todo-build.md`. The first names the continuous goals; the second holds the
implementation work that can be executed by the build loop.
0c. Read `AGENTS.md` and the design documents, source, tests, and drives named by the current standing groups.
0d. **This session runs the procedure itself.** The main thread owns the group audits, task reconciliation,
file writes, validation, commit, and push. Subagents may inspect code and run focused checks, but they never
write task files or decide what gets merged. **You own every child you start**: hold its id, wait for every
result, and let nothing outlive this pass.

## 1. Execute the procedure, in order

1. **Prune `todo-build.md`** — read the whole file and delete every item that is marked DONE,
  in every spelling the file uses (§1a). Only DONE is deleted: `SETTLED`, `NOT A GAP`, `SUPERSEDED` and `BLOCKED`
  are standing decisions that stop settled questions being re-opened, and they survive every prune. Say how many you removed,
   and prove the file is clean the way §1a demands before you move on.
2. **Audit every standing group.** Start one implementation-audit subagent for each group in parallel with
   the others. Give each the exact group heading, purpose, standing tasks, current focus, and acceptance
   expectations from `todo-plan.md`. It must walk every relevant source module, public path, test, drive,
   integration, and failure path, then compare the actual code with `todo-plan.md`, the named design
   documents, and `todo-build.md`.
3. **Require evidence from every audit.** The subagent may run focused commands such as
   `rg -n "TODO|OPEN|Status|Test|Drive|blocked|deferred" <paths>`,
   `git diff -- todo-build.md`, and the narrowest relevant test or build command. It must return a compact
   report containing the group, complete task proposal, comparison of required versus actual behavior,
   exact paths or symbols, existing evidence, dependencies, acceptance test and drive, failure behavior,
   and status: `built`, `partly built`, `blocked`, `deferred`, or `not measured`.
4. **Reject incomplete proposals.** A proposal for `todo-build.md` must be one complete, coherent
   implementation outcome sized for one build iteration. Never accept a half-assed, partial, placeholder,
   or TODO-only proposal. If the complete outcome cannot be executed, record the blocker and its evidence
   rather than disguising it as a smaller task.
5. **Reconcile in the main thread.** After every audit returns, compare its proposals with the existing
   queue and remove duplicates. Reject claims without code or test evidence. Add accepted tasks to
   `todo-build.md` yourself, always as Markdown bullet points, with affected paths, the complete next step,
   dependencies, acceptance test, drive, and failure behavior. Keep the standing groups and their long-term
   goals in `todo-plan.md`.
6. **Review the generated queue.** Read every new bullet, check that it is actionable and complete, and
   confirm no accepted item is merely a partial implementation. Record proposal counts and important
   disagreements in `ralph/progress.txt`.
7. **Prune again** using §1a as the last task-file edit. Prove the remaining DONE hits are prose rather than
   item markers. Do not delete `SETTLED`, `NOT A GAP`, `SUPERSEDED`, or `BLOCKED` decisions.
8. **Validate the plan.** Confirm dependencies, acceptance tests, drives, and current blockers. Plan mode
   writes Markdown only; do not implement source or tests here.
9. **Commit absolutely everything and push to `main`.** Append the final progress block first, run
   `git add -A`, commit with a message describing the planning changes, and push. Confirm the remote tip
   matches the local tip and the working tree is clean. Do nothing after the promise.

## 1a. What a DONE item is, and what clean means

An item is DONE when its first line carries the word DONE as its marker, regardless of whether it is written
as `DONE:`, `- DONE —`, `- **DONE** —`, or another format. The marker line and every continuation line up to
the next blank line, item, or heading are one item and must be removed. Remove empty headings left behind.
Only DONE items are removed. Standing decisions remain.

Prove the prune by searching `todo-build.md` for `DONE` in every case and reading every hit. Each remaining
hit must be prose, never an item marker. Record the real counts and before/after line totals in
`ralph/progress.txt`.

## Standing rules

- Read the entire procedure before acting. Do not hand the whole pass to a subagent.
- Audit every standing group each pass, even when its last audit found no new work.
- Search before declaring anything absent. Separate built, partly built, blocked, deferred, and not measured.
- The main thread owns task wording, priority, completeness, acceptance criteria, and writes to both queues.
- Every accepted `todo-build.md` item is a Markdown bullet describing a complete implementation outcome.
- Never use a task entry to hide uncertainty, an owner decision, missing evidence, or unfinished integration.
- Keep the standing groups continuous. Completing a queue item does not delete its group or its future work.
- Preserve exact failure behavior and validation requirements from the relevant design documents.
- Do not claim completion from a unit test when the requirement is hardware, timing, or cross-process proof.
- A stalled run gets bounded diagnostics before behavioral changes. A timeout is never success.
- Keep the repository clean and include all markdown, progress, and queue changes in the same commit.

## Progress Logging — Mandatory

`ralph/progress.txt` has two jobs: it is the watchdog's only liveness signal, and it is the user's live view of what you are doing (the file is tailed in their terminal). Append with `printf '\n%s\n' "<one-liner>" >> ralph/progress.txt` so each entry sits on its own blank-led line.

The first thing you do is append:
```
═══════════════════════════════════════════════════════
  Ralph (Plan Mode)
═══════════════════════════════════════════════════════

Brief explanation of what you will do (starting with a verb like "Analysing baud and auditing every queued group...", ending in ...)

```
The first line appended must be exactly "═══════════════════════════════════════════════════════".

After the spec is re-derived, append:
```

Specs re-derived. Auditing N groups: <list>.
```
Then narrate as you go — each analysis, the differ, each brief as it reports, the RULINGS review, the verify. Lean toward narrating more rather than less; silence looks like a stall.

After the pass is finished, append the block BELOW FIRST, THEN commit so it is part of the same commit:
```

## <the current UTC time, resolved — e.g. 2026-08-19T03:48:01> UTC - Plan pass committed.
- How many items each step and each brief filed
- Whether verify was green
- **Most severe findings:**
  - [finding 1]
  - [finding 2]
---
```
Resolve the timestamp yourself — run `date -u +%Y-%m-%dT%H:%M:%S` and paste the result. Do NOT append the literal `$(date ...)`: this block is written through Write/Edit, where no shell expansion happens, so a command substitution lands in the log verbatim.

## Stop Condition

After the commit is pushed and confirmed landed, reply with `<promise>COMPLETE</promise>`

Do not perform any additional work after the promise.

## What completion means in plan mode

A planning pass is complete only after the selected task has been fully studied, the standing groups and
implementation queue accurately describe the next work, all required markdown changes are written, and the
commit and push have succeeded. A pass that merely reads the files, drafts a partial outline, or leaves an
unresolved contradiction is not complete.
