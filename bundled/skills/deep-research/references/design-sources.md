# Anthropic research-prompt adaptation

Baseline inspected 2026-10-07. This is a written adaptation of the public
research prompts to Bladerunner's tool/runtime contract and evidence artifacts,
not a claim to reproduce Anthropic's proprietary production system.

| Primary source | Guidance adopted |
|----------------|------------------|
| [Research lead](https://github.com/anthropics/claude-cookbooks/blob/main/patterns/agents/prompts/research_lead_agent.md) | Question/complexity assessment; team-size tiers; distinct task briefs; dependency-aware parallel delegation; iterative follow-ups; lead-owned synthesis; stop when further work adds little |
| [Research subagent](https://github.com/anthropics/claude-cookbooks/blob/main/patterns/agents/prompts/research_subagent.md) | Task-sized retrieval budgets; adaptive queries; inspect underlying sources; appraise reliability; condensed findings with uncertainty |
| [Citation agent](https://github.com/anthropics/claude-cookbooks/blob/main/patterns/agents/prompts/citations_agent.md) | Source-grounded citation placement after synthesis; preserve narrative and cite meaningful assertions |
| [Engineering article](https://www.anthropic.com/engineering/multi-agent-research-system) | Orchestrator/worker architecture, effort scaling, observation of actual behavior, and outcome-oriented evaluation |

The cookbook is [MIT licensed](https://github.com/anthropics/claude-cookbooks/blob/main/LICENSE).
The skill uses original wording; retain upstream notices if incorporating literal
upstream material later. The article and examples are design guidance, not proof
that Bladerunner's implementation improves research quality.

## Bladerunner adaptations

- Use actual task/wait tools rather than upstream `run_blocking_subagent` or
  `complete_task`. The lead coordinates a flat child tree and configured admission.
- Normalize the researcher's differing tool-call figures into task-sized budgets;
  do not claim that prompt limits cause runtime termination.
- Select relevant authorized internal tools; availability alone does not grant
  permission to search unrelated private information.
- Preserve a claim/source ledger and run record so findings can be inspected.
- Citation workers return unsupported assertions for lead correction rather than
  silently forcing citations onto unsupported prose.
- Honor user-chosen formats; provide Markdown and evidence, with PDF when chosen
  and supported. Verify delivered PDF pages.
- Completion follows evidence, answer coverage, citations, and chosen artifacts,
  not fixed numbers of agents, sources, questions, or research waves.

## Validation boundary

Offline scripts check bookkeeping and completion consistency. Independent dry
runs test routing and budget decisions. Neither establishes factual quality.
Live evaluations should span broad comparisons, difficult disputed questions,
and source-sparse topics, measuring answer correctness, citation entailment,
coverage, counterevidence, tool compatibility, time/cost, and clean termination.
