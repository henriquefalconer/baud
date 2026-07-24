export const meta = {
  name: 'ralph-validate',
  description: 'Serially run opus computer-use investigators over todo.md, fixing filed findings per round, until no unchecked items remain',
  whenToUse: 'Run the ralph UI-validation campaign: one opus computer-use session per todo.md item, with a per-round fix pass for findings filed in ralph/ui_problems.md, looping until every todo bullet is checked.',
  phases: [
    { title: 'Investigate', detail: 'one opus session per round, verbatim ralph prompt', model: 'opus' },
    { title: 'Check Findings', detail: 'haiku: did this round file Item-N problems that still need fixing?' },
    { title: 'Fix', detail: 'opus: fix open Item-N findings, validate with computer use, mark SUPERSEDED', model: 'opus' },
    { title: 'Check', detail: 'count unchecked bullets in todo.md' },
  ],
}

const INVESTIGATOR_PROMPT = `pick the most important item to do from @todo.md - your task is to drive the computer use tool calls to verify all functionality UI elements and discover everything that is wrong
IMPORTANT:
- never consider funcionality is already validated
- after todo.md is updated, commit changes and write the problems encountered in ralph/ui_problems.md and commit changes. after that, close any chrome tabs used during session`

const fixPrompt = (itemNumber) => `study ralph/ui_problems.md for Item ${itemNumber} bugs
fix those findings, then validate with computer use tool calls
do not assume already validated
then update ralph/ui_problems.md with changes, mark corresponding as SUPERSEDED and commit changes. after that, close any chrome tabs used during session`

const findingsCheckPrompt = (report) => `A UI-validation session just finished in the repo /Users/vm/git-ocean. Its final report is quoted below.

1. Determine which todo item number this session validated — the "Item N" from todo.md it worked on. Use the report below, the recent git log, and the newest sections of ralph/ui_problems.md to identify it.
2. Read ralph/ui_problems.md and decide whether it records problems for that Item N that STILL NEED FIXING — i.e. findings for that item that are NOT marked SUPERSEDED (or otherwise explicitly marked fixed/resolved).
3. Return structured output: itemNumber (integer, or null if undeterminable), needsFix (true only if unfixed/non-SUPERSEDED findings for that item remain), openFindings (short one-line titles of each still-open finding).

Session report:
<report>
${report}
</report>`

const FINDINGS_SCHEMA = {
  type: 'object',
  properties: {
    itemNumber: { type: ['integer', 'null'], description: 'the todo.md Item number this validation session covered, null if undeterminable' },
    needsFix: { type: 'boolean', description: 'true if ralph/ui_problems.md has findings for this item not yet fixed/SUPERSEDED' },
    openFindings: { type: 'array', items: { type: 'string' }, description: 'one-line titles of the still-open findings' },
  },
  required: ['itemNumber', 'needsFix', 'openFindings'],
}

const COUNT_PROMPT = `In the repo at /Users/vm/git-ocean, count how many lines in todo.md are unchecked markdown checkboxes, i.e. lines whose content starts with "- [ ]" (dash, space, open bracket, space, close bracket). Use grep -c or equivalent via Bash. Return only the count as structured output.`

const COUNT_SCHEMA = {
  type: 'object',
  properties: { unchecked: { type: 'integer', description: 'number of unchecked "- [ ]" bullet lines in todo.md' } },
  required: ['unchecked'],
}

// A schema agent that never calls StructuredOutput makes agent() throw, which
// previously killed the whole run. Catch, retry once at medium effort, and
// return null so callers degrade gracefully instead of crashing the loop.
const structured = async (prompt, opts) => {
  try {
    const r = await agent(prompt, opts)
    if (r != null) return r
    log(`${opts.label}: returned no result; retrying once`)
  } catch (e) {
    log(`${opts.label}: structured output failed (${String(e).slice(0, 140)}); retrying once`)
  }
  try {
    return await agent(prompt, { ...opts, effort: 'medium', label: `${opts.label}:retry` })
  } catch (e) {
    log(`${opts.label}: retry also failed (${String(e).slice(0, 140)}); continuing without result`)
    return null
  }
}

const rounds = []
let remaining = Infinity
let stall = 0
let countFailures = 0

while (remaining > 0) {
  const n = rounds.length + 1
  const result = await agent(INVESTIGATOR_PROMPT, {
    model: 'opus',
    label: `investigate:round-${n}`,
    phase: 'Investigate',
  })
  if (result === null) {
    log(`Round ${n}: investigator was skipped or died; stopping loop`)
    break
  }
  const round = { round: n, summary: String(result).slice(0, 2000), item: null, fixed: false, stillOpen: [] }
  rounds.push(round)

  // Did this validation session file findings that still need fixing?
  const findings = await structured(findingsCheckPrompt(round.summary), {
    model: 'haiku',
    effort: 'low',
    label: `check-findings:round-${n}`,
    phase: 'Check Findings',
    schema: FINDINGS_SCHEMA,
  })
  round.item = findings?.itemNumber ?? null

  if (findings == null) {
    log(`Round ${n}: findings checker unavailable — skipping fix pass this round`)
  } else if (findings.needsFix && findings.itemNumber != null) {
    log(`Round ${n}: Item ${findings.itemNumber} has ${findings.openFindings.length} open finding(s) — launching fix agent`)
    await agent(fixPrompt(findings.itemNumber), {
      model: 'opus',
      label: `fix:item-${findings.itemNumber}`,
      phase: 'Fix',
    })
    round.fixed = true

    // Verify the fix pass actually closed the findings (marked SUPERSEDED).
    const recheck = await structured(findingsCheckPrompt(round.summary), {
      model: 'haiku',
      effort: 'low',
      label: `recheck-findings:round-${n}`,
      phase: 'Check Findings',
      schema: FINDINGS_SCHEMA,
    })
    if (recheck?.needsFix) {
      round.stillOpen = recheck.openFindings
      log(`Round ${n}: after fix pass, Item ${findings.itemNumber} still has open finding(s): ${recheck.openFindings.join('; ')} — continuing to next round`)
    }
  } else if (findings.needsFix) {
    log(`Round ${n}: open findings reported but item number undeterminable — skipping fix pass`)
    round.stillOpen = findings.openFindings
  }

  const check = await structured(COUNT_PROMPT, {
    model: 'haiku',
    effort: 'low',
    label: `check:round-${n}`,
    phase: 'Check',
    schema: COUNT_SCHEMA,
  })
  if (check == null) {
    countFailures++
    if (countFailures >= 3) {
      log(`Count checker failed ${countFailures} rounds in a row; stopping loop`)
      break
    }
    log(`Round ${n} done — count unavailable (checker failed); assuming ${remaining} unchecked remain`)
    continue
  }
  countFailures = 0
  const prev = remaining
  remaining = check.unchecked
  log(`Round ${n} done — ${remaining} unchecked item(s) remain in todo.md`)

  // Sessions can legitimately file new findings, but if the count fails to
  // drop 3 rounds in a row the loop is spinning, not progressing — bail out.
  stall = remaining >= prev ? stall + 1 : 0
  if (stall >= 3) {
    log(`No net progress for ${stall} consecutive rounds (still ${remaining} unchecked); stopping loop`)
    break
  }
}

return { roundsRun: rounds.length, uncheckedRemaining: remaining, stalledOut: stall >= 3, rounds }
