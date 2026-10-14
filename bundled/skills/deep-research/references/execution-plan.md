# Adaptive execution and run record

## Plan for the question

Create a dedicated output directory without replacing previous reports. Save
`protocol.md` (question, scope, cutoff, source-selection and confidence criteria,
user constraints), `plan.json` (answerable subquestions, dependencies, allocation
and intended deliverables), and `run.json` (actual assignments and results).
Keep raw results under `handoffs/`; the lead owns shared files.

Select independent evidence streams for breadth-oriented questions and distinct
methods or explanations for depth-oriented questions. Determine dependencies
before dispatch. The union of assigned work must address the user's question;
multiple researchers may own different perspectives on the same subquestion.
Do not force eight questions, a four-person team, or a source-count target.

## Budgets and follow-ups

Record any user time, source, spending, concurrency, assignment, or round limits
before launch. Optional numeric limits in `run.json` are null when unset; the
absence of a numeric cap does not justify endless work. Use the complexity
heuristics in `SKILL.md`, expected information value, and diminishing returns.
Reassess and save a decision after every completed research wave. Stop once the
answer is adequately supported, another wave is unlikely to improve it, or a
budget/access limit prevents further progress. Further waves do not require a
new user instruction if they remain within the authorized research task/budget.

Select task-sized retrieval budgets, often around 5-15 substantive tool calls,
with smaller allocations for direct fact retrieval and more for difficult work.
Give an explicit stop condition and deadline where relevant. Do not force a
minimum number of searches or page opens. Count actual calls only if observable;
unknown usage is null, never an invented estimate. A prompt budget is not an
API-enforced limit. When a researcher returns partial results, the lead may
commission a new bounded assignment if it is valuable and the overall budget
permits; never extend a worker silently past an explicit hard limit.

Every launch, resume, retry, or swarm member is a new assignment, including failed
work. Reviewer/citation assignments also consume the user's total assignment
budget; the team-size table describes research allocation rather than exempting
other roles from accounting. More than 20 research assignments needs a specific
necessity recorded in the plan/decisions, not merely a desire for more agents.
Respect the runtime's admitted concurrency and schedule queued waves; team size
is not simultaneous execution. Keep models and permission settings unchanged.

Reserve synthesis/citation/QA effort before using the remaining research budget.
Under hard deadlines, use bounded waits, check time before dispatch, and cancel
late work where supported. If lifecycle controls cannot meet the deadline, choose
smaller or foreground bounded work and disclose the limitation. For a global
source/spending cap, partition disjoint allowances or retrieve centrally so
concurrent workers cannot each consume the entire remaining budget.

## Coverage and evidence

Every planned subquestion needs an answer mapped to final ledger claim IDs or a
concrete evidence gap. Compare coverage after each wave; rank follow-ups by their
potential to change the answer. Useful triggers include missing decision criteria,
unresolved credible disagreements, untested alternative explanations, inaccessible
pivotal evidence, or uncertain measurements/methods/currency. Do not rerun an
already satisfied overview. Follow-up prompts contain the missing evidence,
existing sources, prior attempts, and what would resolve the question.

Merge verified DOI/URL duplicates and remap references. The source-cap auditor
counts URLs with normalized scheme, host, default port, and empty root path;
fragments are ignored, while non-root paths and queries remain distinct. Unverified optional
DOI strings do not merge different URLs. Preserve source lineage and actual
inspection provenance; multiple agents finding one study do not create independent
confirmation. A lone authoritative source can be adequate for a narrow official
fact. A causal or contested conclusion may need independent corroboration and
active contradiction testing. Judge adequacy for the claim, not by a source quota.

## Run record

This is an illustrative bounded record, not evidence that work occurred:

```json
{
  "schema_version": 2,
  "completion_status": "bounded",
  "stop_reason": "access",
  "limits": {"max_assignments": null, "max_research_rounds": null,
             "max_sources": null},
  "subquestions": [{"id": "Q1", "question": "What outcome is measured?"}],
  "lead_question_ids": [],
  "assignments": [
    {"id": "A1", "task_id": "actual-runtime-id", "role": "researcher",
     "round": 1, "status": "completed", "question_ids": ["Q1"],
     "tool_call_budget": 10, "tool_calls_used": null}
  ],
  "coverage": [{"question_id": "Q1", "claim_ids": ["C1"],
                "gap": "Pivotal replication inaccessible"}],
  "research_decisions": [{"after_round": 1, "action": "synthesize",
                          "reason": "No accessible route to the missing study"}],
  "stages": {
    "planning": {"status": "completed"},
    "research": {"status": "completed"},
    "synthesis": {"status": "completed"},
    "citations": {"status": "completed", "performed_by": ["LEAD"],
                  "substitution_reason": "No remaining independent assignment budget"},
    "artifact_qa": {"status": "completed"}
  },
  "artifacts": {"markdown": true, "evidence_ledger": true,
                "pdf_generated": false, "pdf_visual_verified": false},
  "required_artifacts": ["markdown", "evidence_ledger"],
  "deviations": [{"rule": "replication access",
                  "reason": "Unretrievable study limits confidence"},
                 {"rule": "independent citation verification",
                  "reason": "No remaining assignment budget; lead checked"}],
  "unresolved_review_findings": []
}
```

Roles: `researcher`, `contrarian`, `evidence_auditor`, `report_reviewer`, or
`citation_verifier`. Only researcher plus citation verification form the usual
pattern; specialist review roles are optional. Assignment status: `completed`,
`failed`, `cancelled`, `running`, or `skipped`. Launched children have actual task
IDs; skipped assignments have a reason and no task ID. Each resume/retry gets a
new assignment ID even if continuing a transcript. Research rounds are positive
integers, not a fixed two-wave pipeline. `lead_question_ids` records exceptional
coverage handled by the lead when delegation is unavailable or inappropriate.

Queries carry `assignment_id`; source `inspected_by` retains all assignment IDs
(comma-separated) or the reserved `LEAD`. Citation-stage `performed_by` records
actual completed citation-verifier IDs or `LEAD`. A lead-only citation check is
not independent verification; record a `substitution_reason` and disclose it. Failed/cancelled work may
supply inspected partial evidence; skipped work cannot supply evidence.

Decision actions: `follow_up` or `synthesize`, with a concrete reason. Record one
or more decisions for every research round, in round order, and finish with
synthesis after the highest research round.
For lead-only retrieval with no launched research children, use `after_round: 0`.
When more than 20 research assignments are necessary, add a specific
`team_expansion_reason`; this is a planning explanation, not a hard ceiling. Stop
reasons: `sufficient_evidence`, `diminishing_returns`, `budget`, `access`, or
`tool_failure`. Required artifacts name formats actually required by the user or
chosen deliverables: `markdown`, `evidence_ledger`, `pdf`. An unavailable optional
PDF is not automatically a completion defect. A requested PDF remains required.

## Completion and verification

- **Full:** scoped questions are addressed (including defensible inconclusive
  answers), facts and citations checked, substantive defects corrected, and chosen
  required artifacts verified. Adequate evidence or diminishing returns explains
  stopping. No minimum team size, source count, or extra review role is required.
- **Bounded:** a usable answer/report and ledger were delivered, but an access,
  budget, or tool limitation materially restricts coverage or verification. Name
  the missing dimension/check and its effect. A cap alone does not lower status
  when the answer was already adequately supported.
- **Incomplete:** known blocking factual/citation defects remain, the answer cannot
  be defended, or a required artifact is missing. Deliver useful partial work and
  state what remains outstanding.

An inconclusive answer is a claim with explicit uncertainty and reasoning, linked
in coverage. A coverage entry with no claims and only a gap is unanswered and
cannot support full completion; disclose it in a bounded or incomplete run.

Record unresolved findings as objects with `id`, `severity` (`blocking`,
`substantive`, `minor`), and concrete `defect`. Factual/citation defects that could
invalidate central conclusions are blocking; stylistic nits are not. Full status
requires correcting substantive findings; minor open presentation notes can be
recorded without claiming false factual uncertainty. Disclose all relevant limits.

Run both audits after final edits. Resolve errors and disposition warnings.
The run auditor checks recorded coverage, provenance, optional limits, decisions,
citation-stage records, and artifact/status consistency. It cannot verify source
truth, search adequacy, actual usage, authorization, or PDF layout. Review pivotal
facts manually; inspect the final rendered pages of every delivered PDF.
