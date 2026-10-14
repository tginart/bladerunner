---
name: deep-research
description: >-
  Conduct comprehensive research through an adaptive team of specialist
  subagents, iterative synthesis, and source-grounded citation verification.
  Use for deep research, literature reviews, technical investigations, or
  evidence-based due diligence; ordinary factual questions do not need this
  workflow unless explicitly requested.
metadata:
  short-description: "Coordinate adaptive research teams and deliver cited reports"
---

# Deep research

Act as the research lead. Delegate substantial retrieval; own the plan,
integration of findings, and final report. Follow the adaptive lead/researcher/
citation pattern in Anthropic's published research prompts, translated to
Bladerunner's tools. [Design provenance](references/design-sources.md).

## Plan and size the team

Determine whether the question needs focused retrieval, broad coverage of
independent subjects, or different approaches to a difficult central problem.
Define the facts needed, scope, evidence cutoff, dependencies, and intended
answer format. Save the plan and chosen budgets before launching.

| Complexity | Researcher guidance |
|------------|---------------------|
| Simple, focused | 1 |
| Standard | 2-3 |
| Medium, multifaceted | 3-5 |
| High, broad or difficult | 5-10, scaling toward 20 when useful |

These are sizing heuristics, not quotas or concurrency settings. For substantial
research, distribute distinct evidence streams or methods across a sufficiently
large team. More than 20 researchers requires a specific necessity and a budget
that supports it; consolidate overlapping work first. Count follow-ups, resumes,
and retries when planning effort. User limits and actual runtime limits govern.

There is no fixed number of subquestions, sources, primary sources, or research
rounds. Choose the team from the work needed to answer this question well.
Read [execution-plan.md](references/execution-plan.md) for planning, budgets, and
run records, and [subagents.md](references/subagents.md) before dispatch.

## Delegate and iterate

Give each researcher a self-contained brief: one substantial objective, context,
scope boundaries, source priorities, available tools, expected output, and a
research budget. Use the templates in `subagents.md`; do not assume inherited
context. Researchers search, retrieve underlying evidence, and return findings,
source locations, conflicts, and unanswered questions. The lead saves handoffs
and merges them using [research-protocol.md](references/research-protocol.md).

Run independent investigations concurrently within admission limits. Complete
prerequisite retrieval before dependent assignments. Depth-oriented work can
combine independent perspectives with successive investigations informed by
previous findings. Larger teams can run in queued waves; children do not delegate.

After each wave, reconcile results and update coverage. Commission targeted
follow-ups, opposing interpretations, or specialist checks when they could
materially improve the answer. Give each new assignment the exact gap and prior
attempts. Independent review is useful when disputed evidence or consequential
claims warrant it; it is not a fixed three-reviewer ceremony.

Stop when the question is adequately supported, further investigation adds
little value, or the available budget is exhausted. Record the actual reason and
remaining gaps. Do not equate a negative search with proof, agent agreement with
independent evidence, or source counts with confidence.

## Synthesize, cite, and deliver

Write the integrated report yourself. Choose the structure and length for the
user's question; distinguish source facts, calculations, and inference. Preserve
contradictions, applicability, confidence reasons, and meaningful limitations.
Read [report-spec.md](references/report-spec.md) before producing artifacts.

Pass the draft and inspected evidence to a separate citation verifier when
subagent tools and budget permit. It attaches citations supported by the sources
and flags assertions it cannot substantiate. Correct those assertions yourself;
do not fabricate a reference or delegate authorship of the final report. When
an independent verifier is unavailable, perform the citation check yourself
and disclose that substitution. Citation checking remains required.

Resolve bookkeeping errors with the bundled audits. Bladerunner substitutes
`${SKILL_DIR}` with this skill's installed directory. In the same shell call,
first set `research_output_dir` to the absolute output directory saved in the
plan. The auditors require Python 3.10 or later; if no compatible interpreter is
available, record the tool limitation and leave the required audit incomplete.
Use the block for the shell actually executing the call.

For Bash/POSIX shells with `python3` available:

```bash
audit_status=0
python3 "${SKILL_DIR}/scripts/audit_evidence.py" "$research_output_dir/evidence.json" || audit_status=1
python3 "${SKILL_DIR}/scripts/audit_research_run.py" "$research_output_dir/run.json" "$research_output_dir/evidence.json" || audit_status=1
exit "$audit_status"
```

For PowerShell (including Windows PowerShell 5.1), set `$research_output_dir`
and run this block. It tries `py -3`, `python3`, then `python`, selecting a
compatible Python installation:

```powershell
$researchPython = $null
$researchPythonArgs = @()
foreach ($candidate in @(
    @{ Name = 'py'; Args = @('-3') },
    @{ Name = 'python3'; Args = @() },
    @{ Name = 'python'; Args = @() }
)) {
    $researchCommand = Get-Command $candidate.Name -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($researchCommand) {
        $researchArgs = @($candidate.Args)
        & $researchCommand.Source @researchArgs -c 'import sys; sys.exit(sys.version_info < (3, 10))'
        if ($LASTEXITCODE -eq 0) {
            $researchPython = $researchCommand.Source
            $researchPythonArgs = $researchArgs
            break
        }
    }
}
if (-not $researchPython) {
    Write-Error 'Research audits require an installed Python 3.10 or later.'
    exit 2
}
$researchAuditStatus = 0
& $researchPython @researchPythonArgs "${SKILL_DIR}/scripts/audit_evidence.py" "$research_output_dir/evidence.json"
if ($LASTEXITCODE -ne 0) { $researchAuditStatus = 1 }
& $researchPython @researchPythonArgs "${SKILL_DIR}/scripts/audit_research_run.py" "$research_output_dir/run.json" "$research_output_dir/evidence.json"
if ($LASTEXITCODE -ne 0) { $researchAuditStatus = 1 }
exit $researchAuditStatus
```

Review and disposition warnings; `--strict` on the evidence auditor is an
optional additional check. These scripts validate recorded structure and
coverage, not factual truth, source entailment, or actual agent execution.

Deliver `report.md` and `evidence.json`, plus `report.pdf` when requested or useful
and generation/inspection tools are available. Inspect any delivered PDF's final
rendered pages. Record completion from answer coverage, performed verification,
and requested artifacts: `full`, `bounded`, or `incomplete` as defined in
`execution-plan.md`. Fewer researchers or sources alone does not reduce status.

## Bladerunner runtime

Use advertised tools and configured models. Bladerunner's default child depth
is one: the lead coordinates all follow-ups. Do not invent task fields or enable
disabled tools/settings. Use bounded waits and available cancellation under hard
deadlines; prompt budgets do not enforce runtime termination. Reserve time for
synthesis, citations, and artifact QA. Return useful partial work with explicit
limits when retrieval, delegation, or requested artifacts remain unavailable.
