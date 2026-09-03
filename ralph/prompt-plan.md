You are in PLAN MODE. The standing task groups in `todo-plan.md` are the continuous goals for this
project. This prompt defines how to advance those groups; it does not contain a one-iteration goal
placeholder.

## Operating contract

`todo-plan.md` is the durable task-group plan. `todo-build.md` is the implementation queue produced and
consumed by the build loop. Read both before doing work. The task groups stay in the plan across passes;
completed implementation items may be collapsed in `todo-build.md`, while the groups themselves remain
available for the next pass.

**Standing task focus.** At the beginning of each planning pass, read the current `todo-plan.md`, identify
the highest-priority group with an actionable unfinished task, and record that group and task in
`ralph/progress.txt`. Do not invent a separate goal, select a smaller substitute, or repeat the same
unfinished task under a new name. If the highest group is blocked, record the blocker and take the first
actionable task in the next group, without hiding the blocker. This is the pass contract; it replaces a
per-entry completion lock while keeping focus continuous and auditable.

At the end of the pass, re-read `todo-plan.md` and `todo-build.md`. Record what was completed, what remains,
and any blocker. Do not claim that a standing group is complete merely because one task in it was completed.

## Scope and permissions

- Plan mode writes markdown only, normally `specs/**`, `todo-plan.md`, and `todo-build.md`.
- Never modify source, tests, configuration, or other non-markdown files from plan mode.
- Before declaring a task missing, search the repository and read the directly relevant implementation,
  tests, drive scripts, and specification files.
- Delegate independent implementation audits in parallel with subagents; keep synthesis and decisions in
  the main pass.
- For every standing group, ask an implementation-audit subagent to drive through each part of the group's
  work in the actual code. Give it the group heading, purpose, standing tasks, and acceptance expectations.
  Instruct it to compare the implementation against `todo-plan.md` and the relevant `specs/**`, inspect the
  source, tests, and drives, and run focused searches or checks such as `rg -n "TODO|OPEN|Status|Test|Drive"
  <paths>`, `git diff -- todo-build.md`, and the narrowest applicable test or build command. It may inspect
  and validate the implementation, but must not edit source, tests, configuration, `todo-plan.md`, or
  `todo-build.md`.
- Require each subagent to return a compact implementation report with: group heading, complete task
  proposal, exact files or symbols, comparison with the specification and plan, evidence of what already
  exists, dependencies, acceptance test and drive, failure behavior, and status (`built`, `partly built`,
  `blocked`, `deferred`, or `not measured`).
- A proposal must describe one complete, coherent implementation outcome sized for one build iteration.
  Never propose a half-assed, partial, placeholder, TODO-only, or knowingly incomplete implementation for
  `todo-build.md`. If the complete outcome is not actionable, report the blocker instead.
- After all reports return, deduplicate them against `todo-build.md`, reject claims without implementation
  evidence, and add accepted complete tasks from the main thread. Every accepted item must be a Markdown
  bullet point in `todo-build.md`, with its affected paths, next step, and acceptance criterion. Preserve
  the standing groups in `todo-plan.md`; subagents audit and propose, but the main thread owns wording,
  priority, completeness, acceptance criteria, and all writes.
- Use one markdown file per logical specification unit when new specifications are needed.
- Cite repository claims with `path:line` when writing specifications.
- Describe what the system must do; describe implementation details only when the task requires them.
- Keep `todo-plan.md` lean and durable: standing groups, dependencies, acceptance expectations, and current
  blockers belong there. Detailed findings and completed implementation history belong in `todo-build.md`
  or `ralph/progress.txt`.
- Keep implementation items sized for one coherent build iteration and ordered by dependency.

## 1. Execute the procedure, in order

1. **Prune `todo-build.md`** — read the whole file and delete every item that is marked DONE, in every spelling the file uses (§1a). Only DONE is deleted: `SETTLED`, `NOT A GAP`, `SUPERSEDED` and `BLOCKED` are standing decisions that stop settled questions being re-opened, and they survive every prune. Say how many you removed, and prove the file is clean the way §1a demands before you move on.

## Pass procedure

1. Read the complete `todo-plan.md` and `todo-build.md`.
2. Read the specifications and repository files named by the selected standing group.
3. Search for existing work before proposing a new item or claiming a capability is absent.
4. Choose the first actionable task under the highest-priority unfinished group, then log the choice.
5. Produce or revise the required markdown plan/specification. Do not implement code in this mode.
6. Validate the plan against dependencies, acceptance tests, drives, and known blockers. Distinguish built,
   partly built, blocked, deferred, and not measured.
7. Re-read both task files. Update only the relevant pending item or standing-group note; never erase an
   unresolved task merely to make the queue appear clean.
8. Append the final Changes committed progress block to `ralph/progress.txt` first, then commit everything
   with `git add -A` and push. Include `todo-plan.md`, `todo-build.md`, progress, and any specification
   changes.
9. After the push, confirm only that the remote tip matches the local tip and the working tree is clean.
   Do not repeat the build gate after pushing.

## Progress logging

`ralph/progress.txt` is the liveness and user-facing record. Append a clear start entry, the selected group
and task, important findings, validation results, and the final Changes committed block. Use UTC timestamps.
Do not leave a plan, question, or unperformed promise as the final state.

## What completion means in plan mode

A planning pass is complete only after the selected task has been fully studied, the standing groups and
implementation queue accurately describe the next work, all required markdown changes are written, and the
commit and push have succeeded. A pass that merely reads the files, drafts a partial outline, or leaves an
unresolved contradiction is not complete.

`<promise>COMPLETE</promise>` is the completion signal for this planning pass. It is not a claim that Baud
is implemented, that every standing group is finished, or that the build loop has no work. Unfinished
standing groups are expected: the next pass re-reads the current plan and continues from its first
actionable task. The signal must be emitted once, only after the final commit and push, and nothing may be
done after it.
