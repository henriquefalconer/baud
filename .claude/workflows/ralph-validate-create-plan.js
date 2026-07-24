export const meta = {
  name: 'ralph-validate-create-plan',
  description: 'Single opus agent authors the todo.md UI-validation plan (computer-use, per-item code citations, ≤75 sessions)',
  whenToUse: 'Regenerate the ralph UI-validation campaign plan: one opus agent studies the codebase and writes todo.md as bullet-point items with cited line numbers, sized for up to 75 Claude sessions.',
  phases: [
    { title: 'Plan', detail: 'one opus agent writes todo.md', model: 'opus' },
  ],
}

const PLAN_PROMPT = `come up with a plan that drives computer use tool calls to validate that every UI element on every page is working 100%, for a different claude session - separately from this. it's important to cite line numbers of code being tested. use up to 75 claude. write todo.md as bullet points`

const result = await agent(PLAN_PROMPT, {
  model: 'opus',
  label: 'create-plan',
  phase: 'Plan',
})

return { report: result }
