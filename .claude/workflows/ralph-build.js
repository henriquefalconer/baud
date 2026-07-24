// ralph-build — interactive-mode emulation of ralph/ralph's build loop
// (per https://github.com/ghuntley/how-to-ralph-wiggum: dumb orchestrator,
// fresh context per iteration, continuity lives on disk in todo.md +
// ralph/progress.txt, backpressure via the repo's verification protocol).
//
// Loop contract (identical to ralph/ralph):
//   - Build agents are spawned LAZILY, one at a time, each with a clean context.
//   - The agent's final message must end with a promise tag (its prompt is the
//     real ralph/prompt-build.md, injected verbatim via @-mention):
//       <promise>NEXT</promise>     -> spawn the next build agent, fresh context
//       <promise>COMPLETE</promise> -> build done, continue to verification
//       no tag                      -> abnormal exit; retry the slot (max 3 in a row)
//
// The prompt files stay the single tuning surface: edit ralph/prompt-build.md
// and the very next iteration follows the new instructions ("tune it like a
// guitar" — no workflow edit needed).
//
// REQUIREMENTS (session-level; a workflow cannot set these itself):
//   Launch the interactive session with:  claude --dangerously-skip-permissions --chrome
//   Agents inherit the session's permission mode and reach the claude-in-chrome
//   MCP tools via ToolSearch for browser + computer use.
//   Subagents only inherit MCP servers the parent session connected, so without
//   --chrome the Verify/Ship phases fall back to CLI + HTTP checks.
//
// TOOLING: every agent here is spawned as the `general-purpose` subagent type
// (tool grant `*`) — the maximum grant a workflow can request. Spawn everything
// through `spawn()` below, never `agent()` directly.
//
//   Workflow subagents cannot spawn their own subagents, so prompt-build.md's
//   Sonnet/Opus subagent steps do not fan out here — iterations run them serially
//   in one context. For a run where they must actually fan out, use `ralph/ralph`
//   (the shell loop): its `claude -p` sessions are top-level and carry Agent.
//
// Invoke:  Workflow {name: "ralph-build"}   or ask: "run the ralph-build workflow"
// Args (all optional):
//   { maxIterations: 0,     // 0 = unlimited, like ./ralph with no count
//     verifyRounds: 10 }    // max verify->fix rounds; 0 = loop until a round is clean

export const meta = {
  name: 'ralph-build',
  description: 'Ralph Wiggum build loop in interactive mode: lazily spawn one clean-context sonnet build agent per todo.md item (prompt injected from @ralph/prompt-build.md), NEXT respawns / COMPLETE advances to Chrome-verified E2E rounds and ship — with per-session token/cost entries in ralph/progress.txt',
  whenToUse: 'Autonomously implement the project specs per todo.md inside an interactive session launched with --dangerously-skip-permissions --chrome. Emulates ralph/ralph build mode without the shell loop.',
  phases: [
    { title: 'Build', detail: 'one clean-context sonnet agent per iteration, driven by @ralph/prompt-build.md; loop on <promise>NEXT</promise>', model: 'sonnet' },
    { title: 'Verify', detail: 'run the app; Chrome + computer-use E2E against each spec domain; collect findings', model: 'sonnet' },
    { title: 'Fix', detail: 'build loop over triaged verification findings, same @ralph/prompt-build.md contract', model: 'sonnet' },
    { title: 'Ship', detail: 'seeds, README quickstart, final verify + Chrome smoke, final usage totals', model: 'sonnet' },
  ],
}

// ─── Config ──────────────────────────────────────────────────────────────────

const MAX_ITER = (args && args.maxIterations) || 0            // 0 = unlimited
const VERIFY_ROUNDS = (args && args.verifyRounds) ?? 10       // 0 = until clean

// The subagent type every agent in this workflow runs as. `general-purpose` is
// declared with `Tools: *`, i.e. the broadest grant requestable: Bash, Read,
// Write, Edit, Skill, Artifact, and ToolSearch — and through ToolSearch every
// deferred and MCP tool (claude-in-chrome browser + computer use, WebFetch,
// WebSearch, Monitor, the Task* family). Agent is the one exclusion and it is
// imposed above this file — see the TOOLING note in the header. Overridable
// per-call via opts.agentType.
const AGENT_TYPE = 'general-purpose'

// Single spawn point: applies AGENT_TYPE to every agent unless a call opts out.
const spawn = (prompt, opts = {}) => agent(prompt, { agentType: AGENT_TYPE, ...opts })

// $/MTok — from the claude-api skill (cached 2026-06-24). Cache read is 0.1x input;
// a 5-minute ephemeral cache write is 1.25x input and a 1-HOUR write is 2x.
// The two write rates are listed separately because this workload writes mostly at
// the 1h TTL: one measured build session wrote 184,954 tokens at 1h against 138,200
// at 5m, so pricing every write at the 5m rate understates cache cost by ~60%.
const PRICING = {
  opus:   { input: 5.00, output: 25.00, cacheRead: 0.50, cacheWrite5m: 6.25,  cacheWrite1h: 10.00 },
  sonnet: { input: 3.00, output: 15.00, cacheRead: 0.30, cacheWrite5m: 3.75,  cacheWrite1h: 6.00 },
  haiku:  { input: 1.00, output: 5.00,  cacheRead: 0.10, cacheWrite5m: 1.25,  cacheWrite1h: 2.00 },
}
const PRICING_TABLE_MD = [
  '| model | input $/MTok | output $/MTok | cache-read $/MTok | cache-write 5m | cache-write 1h |',
  '|---|---|---|---|---|---|',
  '| opus (claude-opus-5) | 5.00 | 25.00 | 0.50 | 6.25 | 10.00 |',
  '| sonnet (claude-sonnet-5) | 3.00 | 15.00 | 0.30 | 3.75 | 6.00 |',
  '| haiku (claude-haiku-4-5) | 1.00 | 5.00 | 0.10 | 1.25 | 2.00 |',
  '',
  'Split cache writes by TTL (usage.cache_creation.ephemeral_1h_input_tokens vs',
  '_5m_) and price each at its own rate. Total prompt size for a request is',
  'input_tokens + cache_read_input_tokens + cache_creation_input_tokens.',
].join('\n')

// ─── Schemas (verification/ship only — build agents speak in promise tags) ──

const FINDINGS_RESULT = {
  type: 'object', additionalProperties: false,
  required: ['domain', 'findings'],
  properties: {
    domain: { type: 'string' },
    findings: {
      type: 'array',
      items: {
        type: 'object', additionalProperties: false,
        required: ['title', 'detail', 'specRef', 'severity'],
        properties: {
          title: { type: 'string' },
          detail: { type: 'string', description: 'what was observed vs what the spec requires; repro steps' },
          specRef: { type: 'string', description: 'spec citation path:line' },
          severity: { type: 'string', enum: ['blocker', 'major', 'minor'] },
        },
      },
    },
  },
}

const OK_RESULT = {
  type: 'object', additionalProperties: false,
  required: ['ok'], properties: { ok: { type: 'boolean' }, note: { type: 'string' } },
}

// ─── Shared prompt fragments ─────────────────────────────────────────────────

const ENV_NOTE = `
## Environment
- Repo: the current working directory. Stack per the specs and CLAUDE.md.
- You run with full permissions (--dangerously-skip-permissions). Chrome may or may not be attached — see the next bullet, do not assume.
- Browser/computer use, IF the session has it: try ONE ToolSearch call ("select:mcp__claude-in-chrome__tabs_context_mcp,mcp__claude-in-chrome__navigate,mcp__claude-in-chrome__computer,mcp__claude-in-chrome__read_page,mcp__claude-in-chrome__tabs_create_mcp"). If it resolves, drive the app in a real tab and use the computer tool for screenshots/clicks; create new tabs, never reuse old tab IDs. If it returns "No matching deferred tools found", the claude-in-chrome MCP server is NOT connected to this session (it requires launching claude with --chrome) — do not search again under other names, and do not treat it as a blocker: verify the same behavior through the CLI and through real HTTP calls against a locally-spawned server (curl + --json output, the drive/*.sh pattern), which is the primary surface of this project anyway. Note in ralph/progress.txt which route you used.
- Full tool grant: Bash, Read, Write, Edit, Skill, and ToolSearch — which loads every deferred and MCP tool on demand (browser/computer use, WebFetch, WebSearch, Monitor, Task*). Use whatever the step needs; nothing is off-limits.
`

// The build prompt is NOT paraphrased here — it is the real prompt file,
// injected via @-mention so edits to ralph/prompt-build.md take effect on the
// very next iteration.
function buildPrompt(iter, extraContext) {
  return `@ralph/prompt-build.md

The instructions injected above (ralph/prompt-build.md) are your complete operating instructions for this session — follow them exactly. If for any reason the file content was not injected, Read ralph/prompt-build.md now and follow it.

Orchestrator notes (they refine, never override, the injected instructions):
- This is Ralph iteration ${iter}. Treat every occurrence of the literal placeholder "[ralph-iteration]" in the injected instructions as ${iter}.
- You are a fresh, clean-context session: all continuity is on disk (todo.md, ralph/progress.txt, git history). Study before assuming.
- There is no shell watchdog in this edition, but the progress-logging protocol is still mandatory — ralph/progress.txt is the user's live view and this run's usage ledger.
- Your final message is read by the orchestrator, which looks ONLY for the promise tag exactly as the injected instructions define: end with <promise>NEXT</promise> or <promise>COMPLETE</promise> and stop. A missing tag is treated as an abnormal exit.
${extraContext ? '\n## Context from the orchestrator\n' + extraContext + '\n' : ''}${ENV_NOTE}`
}

// Extract the LAST promise tag from the agent's final message.
function parsePromise(text) {
  if (typeof text !== 'string') return null
  const matches = [...text.matchAll(/<promise>(NEXT|COMPLETE)<\/promise>/g)]
  return matches.length ? matches[matches.length - 1][1] : null
}

// ─── Usage ledger ────────────────────────────────────────────────────────────
// budget.spent() = output tokens across the whole run; build agents run strictly
// sequentially, so deltas attribute per session. Each ledger entry covers every
// session since the previous entry (including the previous haiku logger itself),
// so ALL sessions of this workspace land in ralph/progress.txt.

let lastSpent = budget.spent()

async function logUsage(phase, label, model) {
  const now = budget.spent()
  const delta = now - lastSpent
  lastSpent = now
  const price = PRICING[model] || PRICING.sonnet
  const outCost = (delta / 1_000_000) * price.output
  await spawn(
    `You are the usage-ledger scribe for an autonomous build run. Append ONE session-usage entry to ralph/progress.txt in the repo root (current working directory). Use bash.

Entry to append (run date yourself; keep this exact shape):

printf '\\n%s\\n' "## \$(date -u +%Y-%m-%dT%H:%M:%S) UTC - Session usage — ${label} [${model}]" >> ralph/progress.txt
printf '%s\\n' "- output tokens (orchestrator-measured, covers all sessions since previous usage entry incl. prior logger): ${delta}" >> ralph/progress.txt
printf '%s\\n' "- est. output cost: \\$${outCost.toFixed(4)} (${model} output rate)" >> ralph/progress.txt

Then, best effort (max 2 tool calls): the session transcript directory for this project under ~/.claude/projects/ (the subdirectory named after the current working directory, with path separators replaced by dashes) contains *.jsonl transcripts whose assistant messages carry usage objects (input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens). If you can quickly sum the usage of the most recently modified agent transcript(s), append one more line with exact totals and an exact cost using this pricing ($/MTok):
${PRICING_TABLE_MD}
Format: "- exact (from transcript): in=<n> out=<n> cache_read=<n> cache_write=<n> → \\$<total>". If it is not quick or the files are ambiguous, append instead: "- exact accounting unavailable this entry". Do nothing else.`,
    { model: 'haiku', effort: 'low', phase, label: `usage:${label}`, schema: OK_RESULT },
  )
}

// ─── Ralph build loop — lazy spawn, promise-tag driven ───────────────────────

let iterCounter = 1

async function buildLoop(phase, extraContext) {
  let consecutiveFailures = 0
  let sessions = 0
  while (true) {
    if (MAX_ITER > 0 && iterCounter > MAX_ITER) {
      log(`Reached max iterations (${MAX_ITER}) without COMPLETE — stopping (ralph exit-1 equivalent).`)
      return { completed: false, sessions }
    }
    if (budget.total && budget.remaining() < 30_000) {
      log('Token target nearly exhausted — pausing the loop.')
      return { completed: false, sessions }
    }

    // Lazy spawn: exactly one clean-context build agent; the next one is only
    // created after this one's final message has been parsed.
    const finalText = await spawn(buildPrompt(iterCounter, extraContext), {
      model: 'sonnet', phase, label: `build:${iterCounter}`,
    })
    await logUsage(phase, `iteration ${iterCounter} (${phase} build session)`, 'sonnet')

    const promise = finalText === null ? null : parsePromise(finalText)
    if (promise === null) {
      // Died on a terminal API error, or finished without a promise tag —
      // ralph's "finished without completing" / retry path.
      consecutiveFailures++
      log(`Iteration ${iterCounter}: ${finalText === null ? 'session died' : 'no promise tag in final message'} (${consecutiveFailures}/3 consecutive failures) — retrying the slot with fresh context.`)
      if (consecutiveFailures >= 3) throw new Error(`Build loop aborted: 3 consecutive abnormal exits at iteration ${iterCounter}`)
      continue
    }
    consecutiveFailures = 0
    sessions++
    log(`Iteration ${iterCounter} [${phase}] — <promise>${promise}</promise>`)
    iterCounter++
    if (promise === 'COMPLETE') return { completed: true, sessions }
    // promise === 'NEXT': loop around and spawn the next agent with clean context.
  }
}

// ─── Phase 1: Build ──────────────────────────────────────────────────────────

phase('Build')
log('Ralph build loop starting: lazily spawning one clean-context sonnet agent per iteration, driven by @ralph/prompt-build.md.')
const build = await buildLoop('Build', '')
if (!build.completed) return { stopped: 'during Build', iterationsRun: iterCounter - 1 }
log(`Build signaled <promise>COMPLETE</promise> after ${build.sessions} sessions. Moving to product verification.`)

// ─── Phase 2+3: Verify (Chrome E2E) → Fix, loop until clean ─────────────────

const DOMAINS_RESULT = {
  type: 'object', additionalProperties: false,
  required: ['domains'],
  properties: {
    domains: {
      type: 'array', minItems: 1, maxItems: 8,
      items: {
        type: 'object', additionalProperties: false,
        required: ['key', 'scope'],
        properties: {
          key: { type: 'string', description: 'short kebab-case domain identifier' },
          scope: { type: 'string', description: 'what the domain covers, plus the spec files it maps to' },
        },
      },
    },
  },
}

const domainsResult = await spawn(`Partition the product surface into verification domains for end-to-end testing.

Read todo.md in full (pending AND completed items — completed items describe what was built) and locate the project's spec documents (todo.md and ralph/prompt-build.md reference where they live). Group the work into 3-8 coherent verification domains, each sized so a single verification agent can exercise it end-to-end in one session. For each domain return:
- key: a short kebab-case identifier (e.g. "core-platform")
- scope: one sentence describing what it covers, followed by the specific spec files it maps to (comma-separated; globs allowed)

Every spec file must belong to exactly one domain. Do not modify any files. Emit {domains: [...]} via StructuredOutput.`,
  { model: 'sonnet', phase: 'Verify', label: 'domains:discover', schema: DOMAINS_RESULT })
const DOMAINS = (domainsResult && domainsResult.domains && domainsResult.domains.length)
  ? domainsResult.domains
  : [{ key: 'full-product', scope: 'the entire product surface — fallback: domain discovery returned nothing' }]
log(`Verification domains (${DOMAINS.length}): ${DOMAINS.map((d) => d.key).join(', ')}`)

let verifyClean = false
for (let round = 1; !verifyClean; round++) {
  if (VERIFY_ROUNDS > 0 && round > VERIFY_ROUNDS) {
    log(`Reached verifyRounds cap (${VERIFY_ROUNDS}) without a clean round — remaining items stay in todo.md.`)
    break
  }
  if (budget.total && budget.remaining() < 30_000) {
    log(`Token target nearly exhausted — stopping before verification round ${round}; remaining items stay in todo.md.`)
    return { stopped: `before Verify round ${round}`, iterationsRun: iterCounter - 1 }
  }
  phase('Verify')
  log(`Verification round ${round}${VERIFY_ROUNDS > 0 ? `/${VERIFY_ROUNDS}` : ''} (until clean): ${DOMAINS.length} domain agents exercising the built product.`)
  // Barrier is intentional: we need ALL domain findings together to dedupe/triage
  // and to early-exit when the round is clean.
  const results = await parallel(DOMAINS.map((d) => () =>
    spawn(`You are a product-verification agent for domain "${d.key}" (round ${round}). Scope: ${d.scope}.

Goal: prove the implemented product actually behaves per the specs — not just that tests pass.
1. Read the domain's specs and the relevant source code (breadth-first yourself — see the no-subagent note below).
2. Run the real thing: the repo's verification protocol first (as defined in todo.md and CLAUDE.md — build + tests); then if the domain has a user-facing surface (app UI, server, CLI), START/LAUNCH IT and exercise the domain's flows end-to-end — use Chrome tools (load via ToolSearch; navigate/read_page/computer for screenshots and clicks) for anything browser-reachable, and computer use for native app UI. If there is no user-facing surface for this domain, exercise the relevant layer directly with small scripts or targeted tests. Honor any testing constraints CLAUDE.md defines (e.g., dedicated test fixtures or throwaway repos).
3. Check the invariants each spec defines for this domain, exactly as written (edge cases, lifecycle rules, error paths included).
4. Report ONLY real, reproducible gaps vs the specs with citations. Do not modify any file. Narrate progress to ralph/progress.txt per the house style (printf '\\n%s\\n' "..." >> ralph/progress.txt), prefixed "[verify:${d.key}]".
${ENV_NOTE}
Emit findings via StructuredOutput; empty findings array means the domain is clean.`,
      { model: 'sonnet', phase: 'Verify', label: `verify:${d.key}`, schema: FINDINGS_RESULT }),
  ))
  await logUsage('Verify', `verification round ${round} (${DOMAINS.length} domain sessions)`, 'sonnet')

  const findings = results.filter(Boolean).flatMap((r) => r.findings.map((f) => ({ ...f, domain: r.domain })))
  const seen = new Set()
  const deduped = findings.filter((f) => {
    const k = `${f.specRef}::${f.title}`.toLowerCase()
    if (seen.has(k)) return false
    seen.add(k)
    return true
  })
  log(`Round ${round}: ${deduped.length} unique finding(s) (${findings.length} raw).`)

  if (deduped.length === 0) { verifyClean = true; break }

  // Triage: fold findings into todo.md as prioritized pending items, commit.
  await spawn(`Triage these verification findings into todo.md as new pending items (prioritized: blockers first, then major, then minor; each scoped to ~one build iteration, with spec citations and repro notes — follow the todo format rules in @ralph/prompt-plan.md section 3). Do not implement fixes. Then append a short progress note to ralph/progress.txt, git add -A, commit ("triage: verification round ${round} findings"), git push.

Findings (JSON):
${JSON.stringify(deduped, null, 2)}

Emit {ok: true} via StructuredOutput when done.`,
    { model: 'sonnet', phase: 'Verify', label: `triage:round${round}`, schema: OK_RESULT })
  await logUsage('Verify', `triage round ${round}`, 'sonnet')

  phase('Fix')
  const fix = await buildLoop('Fix', `This is a FIX round: todo.md was just repopulated with verification findings from round ${round}. Work them top-down exactly like normal todo items.`)
  if (!fix.completed) return { stopped: 'during Fix', round, iterationsRun: iterCounter - 1 }
}

// ─── Phase 4: Ship ───────────────────────────────────────────────────────────

phase('Ship')
const shipText = await spawn(`@ralph/prompt-build.md

The instructions injected above define the house protocol (progress logging, commit discipline, promise tags). This is Ralph iteration ${iterCounter} — treat "[ralph-iteration]" as ${iterCounter}. Your one step for this session is SHIPPING rather than a todo item:

1. Ensure a from-scratch setup path exists and works (dependencies, build, any seed/fixture step the specs call for); document it.
2. Write/refresh README.md: what this is, prerequisites, install/build, run, test/verify commands, project layout. Verify every command by actually running it.
3. Run the repo's full verification protocol (per todo.md and CLAUDE.md) — must be green.
4. Full smoke pass: launch the app and walk one realistic end-to-end flow across the product's main domains (per the specs), with screenshots via computer use (Chrome tools for anything browser-reachable). Honor any testing constraints CLAUDE.md defines (e.g., dedicated test fixtures or throwaway repos). Fix small issues you hit; anything larger goes to todo.md.
5. Follow the injected protocol for progress blocks, commit (git add -A), push, and post-push verification.

End with <promise>COMPLETE</promise> if everything shipped green, else <promise>NEXT</promise>.
${ENV_NOTE}`,
  { model: 'sonnet', phase: 'Ship', label: 'ship' })
const shipPromise = parsePromise(shipText)
iterCounter++
await logUsage('Ship', 'ship session', 'sonnet')

// Final ledger entry: run totals.
const totalOut = budget.spent()
await spawn(`Append a final run-total entry to ralph/progress.txt via bash:
printf '\\n%s\\n' "## \$(date -u +%Y-%m-%dT%H:%M:%S) UTC - RUN TOTALS (ralph-build workflow)" >> ralph/progress.txt
printf '%s\\n' "- total output tokens this run (all sessions): ${totalOut}" >> ralph/progress.txt
printf '%s\\n' "- iterations run: ${iterCounter - 1}; per-session entries above carry the breakdown" >> ralph/progress.txt
Then git add -A && git commit -m "ralph-build: run totals" && git push. Nothing else.`,
  { model: 'haiku', effort: 'low', phase: 'Ship', label: 'usage:run-totals', schema: OK_RESULT })

return {
  buildCompleted: build.completed,
  verifyClean,
  shipped: shipPromise === 'COMPLETE',
  iterationsRun: iterCounter - 1,
  outputTokensSpent: totalOut,
}
