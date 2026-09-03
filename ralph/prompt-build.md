HARD STOP: one iteration = one forward step against @todo-build.md, where a step is the batch of the 10 most important items. A step is complete when every chosen item is resolved in code and the verification protocol defined in @todo-build.md reports green on the pushed main tip. When the step is closed, append the final progress block, emit a single promise (`<promise>NEXT</promise>` if @todo-build.md still has pending items, `<promise>COMPLETE</promise>` if it has zero pending items), and stop. Owner-blocked points live in `todo-plan.md`, never in @todo-build.md, and never count as pending.

**Selection lock.** At the beginning of the main session, record the exact ten entries selected from `@todo-build.md`. They are this iteration's contract. Do not substitute a smaller slice, relabel a partial implementation as the item, or rank the same still-pending item again in a new iteration. Before the final progress block, commit, or promise, re-read `@todo-build.md` and check every entry you selected. Every selected entry must now be completely addressed and collapsed to its own `DONE` line. If any selected entry is still pending or lacks `DONE`, continue the same session and finish it. Only after all selected entries are implemented, validated by the existing procedure, and updated in `@todo-build.md` may you append the final block, commit, push, and return a promise tag.

0a. Study `specs/*` with multiple subagents to learn the application specifications.
0b. Study @todo-build.md.

1. Pick the 10 most important items to address from @todo-build.md (fewer only if @todo-build.md has fewer than 10 pending items; owner-blocked points sit in `todo-plan.md`, never in @todo-build.md, and are never eligible or counted). Work them in one iteration, resolving each completely before moving to the next. Before making changes, search the codebase with subagents (don't assume something is not implemented). Use multiple subagents for searches/reads and a single subagent for build/tests. Use subagents for complex reasoning (debugging, architectural decisions).
2. Author property-based or unit tests, whichever is best.
3. Use chrome use and computer use as needed.
4. Run the tests for the unit of code that was changed - including integration tests. If functionality is missing, add it per the specifications. Ultrathink.
5. When you discover an issue, add a one-line pending entry to @todo-build.md (what · where · next step); put the detail in ralph/progress.txt. The moment an item is resolved, mark it DONE in @todo-build.md with a single-sentence one-liner of what was done (no outcome paragraph).
6. Before committing, run the pre-push validation @todo-build.md defines, exactly as it defines it. Iterate on root causes until it is green. Do not bypass hooks, weaken the commands, or substitute your own sequence for the one @todo-build.md specifies. Give it a generous `timeout: 600000`; if it cannot finish inside one call, re-run it with `run_in_background: true` rather than splitting it into pieces that lose the run. A reported failure is not automatically a regression — re-run that unit in isolation before concluding, and report a flake as a flake with both results, but never work around a failure that reproduces.
7. Append the "Changes committed" progress block to `ralph/progress.txt` first, then commit absolutely everything (`git add -A`, including `ralph/progress.txt`, `todo-build.md`, and `ralph/.last-branch` alongside your code changes) with a message describing the changes. `git push`.
8. Post-push verification is ONLY confirming the push landed: remote branch tip == local tip, working tree clean. Do NOT rebuild or re-run tests/checks — git push doesn't change the code, so pre-push validation (step 6) still holds; re-verifying the same commit is wasted time. Re-run checks post-push ONLY if the project deploys to a live target that can diverge from source (then verify that target). If the push failed, fix it or note it in @todo-build.md. Emit the promise once the push is confirmed landed.
9. Do not write "Iteration NN" anywhere in @todo-build.md.

9999. Important: You can study the specifications and follow the citations to reference source code.
99999. Important: When authoring documentation, capture the why — tests and implementation importance.
999999. Important: Single sources of truth, no migrations/adapters. If tests unrelated to your work - including integration tests - fail, resolve them as part of the increment.
9999999. You may add extra logging if required to debug issues.
99999999. @todo-build.md is terse: OPEN items ≤ ~6 lines each (what · where/citations · next step · acceptance), and RESOLVED items collapsed to a one-line DONE marker (single sentence). All narrative — reasoning, measurements, what you tried, outcomes — goes to ralph/progress.txt, NEVER into @todo-build.md. Future work reads open items here and history there.
999999999. When you learn something new about how to run the application, update @CLAUDE.md using a subagent but keep it brief. For example if you run commands multiple times before learning the correct command then that file should be updated.
9999999999. For any bugs you notice, resolve them or document them in @todo-build.md using a subagent even if it is unrelated to the current piece of work.
99999999999. Implement functionality completely. Placeholders and stubs waste efforts and time redoing the same work.
999999999999. Every iteration, collapse each item you resolved to a single-sentence DONE line (drop any outcome paragraph), and collapse any open item that has grown past ~6 lines back to its essence. @todo-build.md never accumulates narrative.
9999999999999. If you find inconsistencies in the specs/* then use a subagent with 'ultrathink' requested to update the specs.
99999999999999. IMPORTANT: Keep @CLAUDE.md operational only — status updates and progress notes belong in @todo-build.md. A bloated CLAUDE.md pollutes every future loop's context.
999999999999999. NEVER emit a single Write over 400 lines. For larger files, create a ≤400-line skeleton with Write, then grow it with Edits. Placeholders are forbidden.
9999999999999999. DONE: emit `<promise>COMPLETE</promise>` only when @todo-build.md has zero pending items and the iteration's post-push verification returned green. Owner-blocked points belong in `todo-plan.md`, are absent from @todo-build.md, and must not hold the loop open. Re-read @todo-build.md before choosing between `<promise>NEXT</promise>` and `<promise>COMPLETE</promise>`. Finishing a task without verification is not complete; it is a NEXT.

## Progress Logging — Mandatory

ralph/progress.txt has two jobs: (a) the watchdog's only liveness signal (45 min without an append, the iteration is SIGTERM'd mid-work), and (b) the user's live view of what you are doing (the file is tailed in their terminal). Append with `printf '\n%s\n' "<one-liner>" >> ralph/progress.txt` so each entry sits on its own blank-led line.

Most importantly, the first thing you should do is append (iteration number should be exactly "[ralph-iteration]"):
```
═══════════════════════════════════════════════════════
  Ralph Iteration [ralph-iteration] (Build Mode)
═══════════════════════════════════════════════════════

Brief explanation of what you will do (starting with a verb like "Finding the 10 most important items to address...", ending in ...)

```
The first line appended should be "═══════════════════════════════════════════════════════". If it's empty, make sure the first line is exactly "═══════════════════════════════════════════════════════".

After picking the items to be addressed, append:
```

Chose these 10 items: X1, X2, ... X10 — they're the Y of Z.
```
Then, as you start each one, append a line naming it so the tail shows which of the 10 is in flight.
The first line appended should be an empty line.

Whenever something meaningful happens, append a short note. Lean toward narrating more rather than less; silence looks like a stall.
```

Found/did/finished X. Now doing/investigating Y...
```
The first line appended should be an empty line.

After important finding, append:
```

Brief explanation of what was done/found. [Then "Continuing task..." or something like that]
```
The first line appended should be an empty line.

After re-reading `@todo-build.md` and confirming every item selected at the beginning of this main session is now a `DONE` entry, append the block BELOW to `ralph/progress.txt` FIRST, THEN run `git add -A` and `git commit` so the block is part of the same commit:
```

## <the current UTC time, resolved — e.g. 2026-08-19T03:48:01> UTC - Changes committed.
- What was implemented
- Files changed
- **Brief description of changes:**
  - [change 1]
  - [change 2]
  - ...
---
```
The first line appended should be an empty line. Resolve the timestamp yourself — run `date -u +%Y-%m-%dT%H:%M:%S` and paste the result. Do NOT append the literal text `$(date ...)`: this block is written through the Write/Edit tools, where no shell expansion happens, so a command substitution lands in the log verbatim.

## Stop Condition

Only after the post-push verification has resolved, every item selected at the beginning of this main session is confirmed `DONE` in `@todo-build.md`, and the final progress block is appended and committed with everything included, reply with one of:

- `<promise>NEXT</promise>` if @todo-build.md still has pending items, or if verification reported cannot-complete. The outer loop will start a fresh iteration.
- `<promise>COMPLETE</promise>` if @todo-build.md has zero pending items (verify by re-reading) and verification returned green. The outer loop will exit.

Owner-blocked points are recorded in `todo-plan.md` and deleted from @todo-build.md, so they count towards neither promise — counting a decision you are not allowed to make as pending answers `NEXT` forever on iterations that can only re-read it.

Do not perform any additional work after the promise. All verification happens before the promise, not after.

## You are operating autonomously

Nothing will resume you: this session ends when you stop, and an unfinished step is lost.

Long commands: split them into one Bash call per step, each with `timeout` (max 600000 ms), and append a progress note before each (silent sessions get terminated) — never chain with `&&`, and never background a command whose result you need: a backgrounded command is killed when the session ends, and a chained `sleep 30; …` wait is refused outright. To wait for something, poll inside ONE call (`until <check>; do sleep 5; done`) or use `Monitor`, which does work here — its events come back as new turns. Never use `ScheduleWakeup`: its delay is clamped to 60s while this session exits in about ten, so the wakeup can never fire, yet it replies that one is scheduled and that there is nothing more to do this turn.

Before ending, re-read your last paragraph. If it is a plan, a question, or a promise about work you have not done ("Waiting for…", "I'll…"), the step is not closed — do it now.

End committed, pushed and tagged — an untagged final message is an abnormal exit that retries the whole iteration — or blocked on a decision that is not yours to make (a product ruling, a naming or scope call, a trade-off the register has not settled), in which case the point goes in `todo-plan.md` on a line beginning `BLOCKED ON THE OWNER —` naming the answer awaited and where it will be written (normally `sculpt/register/RULINGS.md`), and is deleted from @todo-build.md entirely — @todo-build.md never mentions the block, not as an item, a marker, a stub or a cross-reference, so it holds only work you can actually do and nothing there is ever a question you are forbidden to answer; never answer such a point yourself, and move it back into @todo-build.md as ordinary work only when something already settles it (RULINGS, CLAUDE.md, or a fact you can measure), citing what does, since recording one is not work and so never by itself justifies `NEXT`.
