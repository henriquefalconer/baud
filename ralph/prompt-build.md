HARD STOP: one iteration = one forward step against @todo.md. A step is complete when the chosen item is resolved in code and the verification protocol defined in @todo.md reports green on the pushed main tip. When the step is closed, append the final progress block, emit a single promise (`<promise>NEXT</promise>` if @todo.md still has pending items, `<promise>COMPLETE</promise>` if it has zero pending items), and stop.

0a. Study `specs/*` with multiple Sonnet subagents to learn the application specifications.
0b. Study @todo.md.

1. Pick the most important item to address from @todo.md. Before making changes, search the codebase with Sonnet subagents (don't assume something is not implemented). Use multiple Sonnet subagents for searches/reads and a single Sonnet subagent for build/tests. Use Opus subagents for complex reasoning (debugging, architectural decisions).
2. Author property-based or unit tests, whichever is best.
3. Use chrome use and computer use as needed.
4. Run the tests for the unit of code that was changed - including integration tests. If functionality is missing, add it per the specifications. Ultrathink.
5. When you discover issues, update @todo.md with your findings using a subagent. Remove items when resolved.
6. Before committing, run any pre-push validation @todo.md defines. Iterate on root causes until it is green. Do not bypass hooks or weaken the commands. Run it as SEPARATE Bash calls — `cargo build`, then `cargo clippy`, then `cargo test`, then each `drive/*.sh` one at a time — each with `timeout: 600000`. Never chain them with `&&` into one command: the whole suite cannot finish inside one call, so it gets force-backgrounded and you lose the run.
7. Append the "Changes committed" progress block to `ralph/progress.txt` first, then commit absolutely everything (`git add -A`, including `ralph/progress.txt`, `todo.md`, and `ralph/.last-branch` alongside your code changes) with a message describing the changes. `git push`.
8. Verify the push against @todo.md's post-push verification protocol. If it reports a failure (deploy broke, regression, the fix did not take), keep iterating in this invocation (more commits allowed, still the same step). If it reports cannot-complete (upstream unreachable, ambiguous signal), record what you observed in @todo.md and emit `<promise>NEXT</promise>`. Only emit a promise once verification has resolved for this iteration.
9. Do not write "Iteration NN" anywhere in @todo.md.

9999. Important: You can study the specifications and follow the citations to reference source code.
99999. Important: When authoring documentation, capture the why — tests and implementation importance.
999999. Important: Single sources of truth, no migrations/adapters. If tests unrelated to your work - including integration tests - fail, resolve them as part of the increment.
9999999. You may add extra logging if required to debug issues.
99999999. Keep @todo.md current with learnings using a subagent — future work depends on this to avoid duplicating efforts. Update especially after finishing your turn.
999999999. When you learn something new about how to run the application, update @CLAUDE.md using a subagent but keep it brief. For example if you run commands multiple times before learning the correct command then that file should be updated.
9999999999. For any bugs you notice, resolve them or document them in @todo.md using a subagent even if it is unrelated to the current piece of work.
99999999999. Implement functionality completely. Placeholders and stubs waste efforts and time redoing the same work.
999999999999. When @todo.md becomes large periodically clean out the items that are completed from the file using a subagent.
9999999999999. If you find inconsistencies in the specs/* then use an Opus 4.6 subagent with 'ultrathink' requested to update the specs.
99999999999999. IMPORTANT: Keep @CLAUDE.md operational only — status updates and progress notes belong in @todo.md. A bloated CLAUDE.md pollutes every future loop's context.
999999999999999. NEVER emit a single Write over 400 lines. For larger files, create a ≤400-line skeleton with Write, then grow it with Edits. Placeholders are forbidden.
9999999999999999. DONE: emit `<promise>COMPLETE</promise>` only when @todo.md has zero pending items and the iteration's post-push verification returned green. Re-read @todo.md before choosing between `<promise>NEXT</promise>` and `<promise>COMPLETE</promise>`. Finishing a task without verification is not complete; it is a NEXT.

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

After picking item to be addressed, append:
```

Chose X, it's the Y of Z.
```
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

After the post-push verification has resolved (green, red-but-worked-through, or cannot-complete) and the final progress block is appended and the commit is done with everything included, reply with one of:

- `<promise>NEXT</promise>` if @todo.md still has pending items, or if verification reported cannot-complete. The outer loop will start a fresh iteration.
- `<promise>COMPLETE</promise>` if @todo.md has zero pending items (verify by re-reading) and verification returned green. The outer loop will exit.

Do not perform any additional work after the promise. All verification happens before the promise, not after.

## You are operating autonomously

Nothing will resume you: this session ends when you stop, and an unfinished step is lost.

Long commands: split them into one Bash call per step, each with `timeout` (max 600000 ms), and append a progress note before each (silent sessions get terminated) — never chain with `&&`, and never background a command whose result you need: a backgrounded command is killed when the session ends, and a chained `sleep 30; …` wait is refused outright. To wait for something, poll inside ONE call (`until <check>; do sleep 5; done`) or use `Monitor`, which does work here — its events come back as new turns. Never use `ScheduleWakeup`: its delay is clamped to 60s while this session exits in about ten, so the wakeup can never fire, yet it replies that one is scheduled and that there is nothing more to do this turn.

Subagents are ASYNCHRONOUS by default. An Agent call without `run_in_background: false` returns a launch acknowledgement in milliseconds — not a result — and the subagent's output arrives later as a separate turn. Pass `run_in_background: false` whenever you need the result to carry on: measured, that returns the finished report inline, against a 13ms "launched successfully" ack by default. Either way you never wait for a subagent by stopping; stopping is what loses the iteration.

Before ending, re-read your last paragraph. If it is a plan, a question, or a promise about work you have not done ("Waiting for…", "I'll…"), the step is not closed — do it now.

End in one of two states: the step is closed, committed, pushed, and tagged; or you are blocked on something only the user can provide — record it in @todo.md and emit `<promise>NEXT</promise>`. Blocked-and-tagged beats no tag: an untagged final message is an abnormal exit and the whole iteration is retried.
