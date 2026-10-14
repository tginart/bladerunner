# Research workers and citation verification in Bladerunner

## Runtime contract

Use the advertised task tool (normally `task`, sometimes `spawn_subagent`).
Bladerunner's default child depth is one: all launches and follow-ups come from
the lead. Children research their assigned objective and return evidence; they
must not invoke task/swarm/workflow tools to create another layer.

The source runtime defaults to 32 concurrent children, with queuing, but the
actual session may have a lower configured limit (`GROK_MAX_CONCURRENT_SUBAGENTS`
and admission settings). Respect live admission; do not impose a skill-specific
four-child cap or change settings to obtain more capacity. Team size is distinct
from concurrency. A large team can be scheduled in smaller waves.

Use `general-purpose` with `capability_mode: "read-only"` when advertised and it
retains the necessary retrieval tools. Short-turn `scout` and library-specific
`librarian` profiles are not general deep-research roles. The task schema supports
`prompt`, `description`, `subagent_type`, `run_in_background`, `capability_mode`,
`isolation`, `resume_from`, `cwd`, and `model`; it does not support per-call
`reasoning_effort`, `max_turns`, or `timeout`. Follow the live schema if different.
Keep configured models unless the user requests a change.

Read-only workers return complete findings for the lead to save. Supply source
text/schema in prompts when workers cannot read shared files. Tool availability
is not permission to gather unrelated private data; select relevant authorized
read-only sources for the question. Do not require file edits, shell installs,
or disabled tools. A missing retrieval tool is a limitation, not a completed
search or evidence of no effect.

For ordinary task dispatch, fill `description`, `subagent_type`, and `prompt`;
add supported background/read-only options. `agent_swarm` can distribute a common
brief across distinct `items` with literal `{{item}}`, but must be the only tool
call in its batch and does not accept task-only capability/isolation fields.
Its maximum size is not a research recommendation.

A task ID is not a result. Collect final output or failure using advertised
lifecycle tools, account for all launched work, and preserve useful partial
results. Resume only a completed child with its actual ID and the same type,
omitting `model`. Resumes/retries each consume a new assignment. A prompt
budget/deadline does not enforce cancellation; bound waits and cancel overruns
under hard deadlines where supported. Do not wait indefinitely.

## Researcher work order

Use this self-contained brief, replacing every placeholder with actual content
and supplying the ledger fragment schema from `research-protocol.md`:

```text
Assignment {ID}; research round {number}.
User question, audience, and decision: {details}.
Your objective: {one substantial evidence stream, method, perspective, or gap}.
Owned questions: {IDs and exact questions}.
Scope/cutoff/definitions: {frozen protocol relevant to this objective}.
Excluded work and dependencies: {boundaries, prerequisite results, overlap}.
Available evidence and prior attempts: {sources, locators, inspected text,
query families; for follow-ups, the exact remaining gap}.
Preferred sources and tools: {task-specific authoritative/original evidence,
relevant authorized internal sources, actual advertised tool names}.
Budget and stop condition: {tool-call allocation, source slots if capped,
deadline, what constitutes an adequate result}.

Plan retrieval around the objective. Begin with useful broad queries, then
narrow and retrieve promising original material. Adapt queries to new evidence;
do not repeat unsuccessful searches or force a fixed number of calls/sources.
Inspect source text rather than treating snippets as evidence. Examine methods,
data/units/dates and applicability when needed to support the claims.
Consider serious alternative explanations and contradicting evidence where
relevant. Separate forecasts, marketing assertions, inference and measured facts.
Record inaccessible material, disagreements, retrieval failures and limitations.
Stop when the objective is addressed, retrieval adds little, or the budget ends.
Do not delegate, edit shared files, or invent research activity.

Return a dense JSON fragment with strand, question_ids, search_log, sources,
claims, conflicts, limitations, and gaps [{question_id, missing_evidence,
impact, suggested_next_step}]. Use this exact ledger schema: {field shapes}.
Each query has assignment_id {ID}; prefix source/claim IDs with {ID}.
Each source has inspected:true, inspected_by:{ID}, and inspection_notes naming
what was actually retrieved/read. Every supporting/counterevidence item has
source_id, a precise locator, and the narrow assertion it supports.
```

Depth-oriented assignments may examine competing mechanisms, methods, or
interpretations; breadth-oriented assignments cover distinct entities or decision
criteria. A follow-up is a new work order with its exact gap and completion
condition, not a repeated general overview. A contrarian or evidence auditor can
use the same handoff while checking specified provisional claims independently.
Do not ask a worker to write the final report; the lead integrates the findings.

## Citation-verifier work order

Dispatch after the lead drafts the report. Give the verifier the actual text,
claim-to-report mapping and inspected source material, not just source titles.

```text
Assignment {ID}; role citation_verifier.
Inputs: {lead-written draft, evidence ledger, inspected text/URLs/locators,
claim-to-report mapping, chosen citation/reference style}.
Budget: {verification allocation/deadline and actual available tools}.

Attach citations only where the supplied sources directly support substantive
assertions, important numbers/dates and conclusions. Prefer citations at complete
sentences or paragraphs; avoid redundant references and fragmented reading.
Check source identity, locators, units/dates, and whether the assertion overstates
what the source shows. Reopen pivotal evidence when available. Never pretend to
have retrieved inaccessible text.
Preserve the lead's narrative while adding/verifying citations. Do not silently
rewrite unsupported claims or invent references. Return the annotated draft,
verified_claim_ids, unchecked_claim_ids, and findings [{id, severity, claim_ids,
source_ids, paragraph_or_page, defect, proposed_fix}], plus limitations.
Severity: blocking (unsupported central fact or erroneous citation), substantive
(material qualification/coverage defect), or minor (presentation).
No shared-file edits or delegation. The lead owns corrections and final delivery.
```

Citation verification is not new primary research by default. If it exposes a
consequential evidence gap, the lead may commission another research wave within
budget, revise the report, and recheck affected citations. Under a small cap the
lead may verify citations directly; record that it was not independent.
Optional report review or specialist audit should target concrete uncertainty,
not add reviewers solely to satisfy a role count.

## Merge and retain provenance

Save complete handoffs before using their claims. If results were truncated,
retrieve missing portions or request smaller handoffs within budget. Merge by
canonical DOI/URL; preserve materially different versions/dates. Preserve all
inspectors in `inspected_by` (comma-separated IDs), assign final S/C IDs, and
update evidence/counterevidence/conflict/coverage references. Keep shared study
lineage in `independence_group`; agreement between agents is not replication.
Keep honest inspection scope: abstracts support only what is visible in them.
Return concise paraphrases, not entire copyrighted sources.

The lead saves query logs and working notes, records actual assignments/status
and decisions in `run.json`, and reopens pivotal support/counterevidence when
needed to confirm the synthesis. Report performed checks and meaningful gaps.
