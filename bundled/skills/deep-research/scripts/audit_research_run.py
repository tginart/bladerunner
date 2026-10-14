#!/usr/bin/env python3
"""Check adaptive research records, optional budgets and completion consistency.

No team/source/round quotas. This checks records, not truth or actual execution.
"""

import argparse
import ipaddress
import json
from pathlib import Path
from urllib.parse import quote, unquote, urlsplit, urlunsplit

from audit_evidence import Audit, validate_url

STAGES = ("planning", "research", "synthesis", "citations", "artifact_qa")
ROLES = {"researcher", "contrarian", "evidence_auditor", "report_reviewer", "citation_verifier"}
RESEARCH_ROLES = {"researcher", "contrarian"}


def member(value, choices):
    return isinstance(value, str) and value in choices


def text(value):
    return isinstance(value, str) and bool(value.strip())


def positive(value):
    return type(value) is int and value > 0


def canonical_source_url(url):
    audit = Audit()
    validate_url(audit, url.strip(), "source.url")
    if audit.findings:
        raise ValueError("source URL is malformed")
    parsed = urlsplit(url.strip())
    scheme = parsed.scheme.lower()
    hostname = parsed.hostname
    if scheme not in {"http", "https"} or not hostname:
        raise ValueError("source URL needs an HTTP(S) hostname")
    hostname = unquote(hostname, errors="strict")
    if ":" in hostname:
        hostname = ipaddress.ip_address(hostname).compressed
        if "%" in hostname:
            address, zone = hostname.split("%", 1)
            hostname = address + "%25" + quote(zone, safe="-._~")
        hostname = "[" + hostname + "]"
    else:
        hostname = hostname.encode("idna").decode("ascii").lower().removesuffix(".")
    userinfo = parsed.netloc.rpartition("@")[0] + "@" if "@" in parsed.netloc else ""
    port = parsed.port
    authority = userinfo + hostname
    if port is not None and port != {"http": 80, "https": 443}[scheme]:
        authority += f":{port}"
    return urlunsplit((scheme, authority, parsed.path or "/", parsed.query, ""))


def audit_run(run, evidence):
    errors, warnings = [], []
    if not isinstance(run, dict) or not isinstance(evidence, dict):
        return ["run and evidence must be JSON objects"], []
    full = run.get("completion_status") == "full"
    if run.get("schema_version") != 2:
        errors.append("schema_version must be 2 (adaptive research record)")
    if not member(run.get("completion_status"), {"full", "bounded", "incomplete"}):
        errors.append("completion_status must be full, bounded, or incomplete")
    if not member(run.get("stop_reason"), {"sufficient_evidence", "diminishing_returns", "budget", "access", "tool_failure"}):
        errors.append("stop_reason must name evidence adequacy, diminishing returns, or an actual limit")
    elif full and run["stop_reason"] in {"budget", "access", "tool_failure"}:
        errors.append("full cannot hide coverage/verification limited by budget, access, or tools")
    elif run.get("completion_status") == "bounded" and run["stop_reason"] not in {"budget", "access", "tool_failure"}:
        errors.append("bounded status needs a budget, access, or tool_failure stop reason")

    def rows(container, key):
        value = container.get(key)
        if not isinstance(value, list) or any(not isinstance(x, dict) for x in value):
            errors.append(f"{key} must be a list of objects")
            return []
        return value

    def obj(key):
        value = run.get(key)
        if not isinstance(value, dict):
            errors.append(f"{key} must be an object")
            return {}
        return value

    def refs(value, known, label):
        if not isinstance(value, list) or any(not text(i) or i not in known for i in value):
            errors.append(f"{label} must refer to known IDs")
            return []
        return value

    def limited(message):
        (errors if full else warnings).append(message)

    limits = obj("limits")
    for name in ("max_assignments", "max_research_rounds", "max_sources"):
        if limits.get(name) is not None and not positive(limits[name]):
            errors.append(f"limits.{name} must be a positive integer or null")

    questions = rows(run, "subquestions")
    question_ids = [q.get("id") for q in questions]
    valid_questions = {q for q in question_ids if text(q)}
    if not questions or len(valid_questions) != len(question_ids):
        errors.append("subquestions need nonempty unique IDs")
    if any(not text(q.get("question")) for q in questions):
        errors.append("each subquestion needs an answerable question")

    assignments = rows(run, "assignments")
    ids = [a.get("id") for a in assignments]
    valid_ids = {i for i in ids if text(i)}
    if len(valid_ids) != len(ids) or "LEAD" in valid_ids:
        errors.append("assignment IDs must be nonempty, unique, and not the reserved LEAD")
    by_id = {a["id"]: a for a in assignments if text(a.get("id"))}
    launched = [a for a in assignments if a.get("status") != "skipped"]
    if positive(limits.get("max_assignments")) and len(launched) > limits["max_assignments"]:
        errors.append("assignment cap exceeded (includes failures, resumes, retries and reviewers)")
    owned = set(refs(run.get("lead_question_ids"), valid_questions, "lead_question_ids"))
    rounds = set()
    research_assignments = []
    for a in assignments:
        label = str(a.get("id", "assignment"))
        if not member(a.get("role"), ROLES):
            errors.append(f"{label}: unknown role")
        if not member(a.get("status"), {"completed", "failed", "cancelled", "running", "skipped"}):
            errors.append(f"{label}: invalid status")
        if a.get("status") == "running":
            errors.append(f"{label}: account for running work before final delivery")
        if a.get("status") == "skipped":
            if a.get("task_id") or not text(a.get("reason")):
                errors.append(f"{label}: skipped assignment needs a reason and no task_id")
        elif not text(a.get("task_id")):
            errors.append(f"{label}: launched assignment needs an actual task_id")
        round_number = a.get("round")
        if not positive(round_number):
            errors.append(f"{label}: round must be a positive integer")
        qs = refs(a.get("question_ids"), valid_questions, f"{label}.question_ids")
        if member(a.get("role"), RESEARCH_ROLES) and a.get("status") != "skipped":
            owned.update(qs)
            research_assignments.append(a)
            if positive(round_number):
                rounds.add(round_number)
                if positive(limits.get("max_research_rounds")) and round_number > limits["max_research_rounds"]:
                    errors.append(f"{label}: research round cap exceeded")
        budget, used = a.get("tool_call_budget"), a.get("tool_calls_used")
        if budget is not None and not positive(budget):
            errors.append(f"{label}: tool_call_budget must be a positive integer or null")
        if used is not None and (type(used) is not int or used < 0):
            errors.append(f"{label}: tool_calls_used must be a nonnegative integer or null")
        elif positive(budget) and type(used) is int and used > budget:
            errors.append(f"{label}: recorded tool-call budget exceeded")
    for q in sorted(valid_questions - owned):
        errors.append(f"{q}: no assigned research or lead coverage")
    if len(research_assignments) > 20 and not text(run.get("team_expansion_reason")):
        errors.append("more than 20 research assignments needs a specific team_expansion_reason")

    claim_ids = {c["id"] for c in rows(evidence, "claims") if text(c.get("id"))}
    covered = []
    for c in rows(run, "coverage"):
        q = c.get("question_id")
        if not text(q) or q not in valid_questions:
            errors.append("coverage references unknown question")
        else:
            covered.append(q)
        linked = refs(c.get("claim_ids"), claim_ids, f"{q}.claim_ids")
        if not linked and not text(c.get("gap")):
            errors.append(f"{q}: unanswered coverage needs a concrete gap")
        elif not linked:
            limited(f"{q}: unanswered coverage limits completion")
    for q in valid_questions:
        if covered.count(q) != 1:
            errors.append(f"{q}: needs exactly one coverage entry")

    def inspector(who, label):
        if who == "LEAD":
            return
        if not text(who) or who not in by_id:
            errors.append(f"{label}: needs a known assignment_id or LEAD")
        elif by_id[who].get("status") == "skipped":
            errors.append(f"{label}: skipped assignment cannot supply evidence")

    for entry in rows(evidence, "search_log"):
        inspector(entry.get("assignment_id"), "search_log")
    inspected = set()
    for source in rows(evidence, "sources"):
        who = source.get("inspected_by")
        inspectors = who.split(",") if text(who) else []
        if not inspectors:
            errors.append(f"{source.get('id')}: missing inspection provenance")
        for who in inspectors:
            inspector(who.strip(), f"{source.get('id')}.inspected_by")
        if source.get("inspected") is True:
            url = source.get("url")
            try:
                key = canonical_source_url(url) if text(url) else ""
            except ValueError:
                errors.append(f"{source.get('id')}: malformed source URL cannot be deduplicated")
                continue
            if key:
                inspected.add(key)
    if positive(limits.get("max_sources")) and len(inspected) > limits["max_sources"]:
        errors.append("global source cap exceeded after recorded URL deduplication")

    decisions = rows(run, "research_decisions")
    decision_rounds = set()
    previous_round = -1
    for d in decisions:
        n = d.get("after_round")
        if type(n) is not int or n not in (rounds or {0}):
            errors.append("research decision must refer to an actual research round (0 for lead-only)")
        else:
            decision_rounds.add(n)
            if n < previous_round:
                errors.append("research decisions must follow round order")
            previous_round = n
        if not member(d.get("action"), {"follow_up", "synthesize"}) or not text(d.get("reason")):
            errors.append("each research decision needs action and concrete reason")
    if decision_rounds != (rounds or {0}):
        errors.append("record a research decision after every round")
    if not decisions or decisions[-1].get("action") != "synthesize":
        errors.append("final research decision must be synthesize")
    elif decisions[-1].get("after_round") != max(rounds or {0}):
        errors.append("final synthesis decision must target the last research round")

    stages = obj("stages")
    for name in STAGES:
        stage = stages.get(name)
        if not isinstance(stage, dict):
            errors.append(f"missing stage record: {name}")
            continue
        if not member(stage.get("status"), {"completed", "skipped", "failed"}):
            errors.append(f"{name}: invalid final stage status")
        elif stage["status"] != "completed":
            if not text(stage.get("reason")):
                errors.append(f"{name}: missing reason")
            limited(f"{name}: stage was {stage['status']}")
    citation_stage = stages.get("citations", {})
    if isinstance(citation_stage, dict) and citation_stage.get("status") == "completed":
        performers = refs(citation_stage.get("performed_by"), valid_ids | {"LEAD"}, "citations.performed_by")
        if not performers:
            errors.append("completed citation check needs actual performers")
        for who in performers:
            if who != "LEAD" and (by_id[who].get("status") != "completed" or by_id[who].get("role") != "citation_verifier"):
                errors.append("citation performer must be a completed citation_verifier assignment or LEAD")
        if set(performers) == {"LEAD"} and not text(citation_stage.get("substitution_reason")):
            errors.append("lead-only citation verification needs a substitution_reason")

    artifacts = obj("artifacts")
    for name in ("markdown", "evidence_ledger", "pdf_generated", "pdf_visual_verified"):
        if type(artifacts.get(name)) is not bool:
            errors.append(f"artifacts.{name} must be boolean")
    if artifacts.get("pdf_visual_verified") and not artifacts.get("pdf_generated"):
        errors.append("PDF cannot be visually verified without being generated")
    if artifacts.get("pdf_generated") and not artifacts.get("pdf_visual_verified"):
        limited("delivered PDF lacks final visual verification")
    required = run.get("required_artifacts")
    if not isinstance(required, list) or any(not member(name, {"markdown", "evidence_ledger", "pdf"}) for name in required):
        errors.append("required_artifacts must list chosen markdown, evidence_ledger, or pdf deliverables")
    else:
        for name in required:
            flag = "pdf_generated" if name == "pdf" else name
            if artifacts.get(flag) is not True and run.get("completion_status") != "incomplete":
                errors.append(f"missing required artifact {name} requires incomplete status")
    if run.get("completion_status") in ("full", "bounded") and any(artifacts.get(name) is not True for name in ("markdown", "evidence_ledger")):
        errors.append("usable research completion needs Markdown and evidence ledger; otherwise use incomplete")

    unresolved = rows(run, "unresolved_review_findings")
    finding_ids = [finding.get("id") for finding in unresolved]
    valid_finding_ids = {identifier.strip() for identifier in finding_ids if text(identifier)}
    if len(valid_finding_ids) != len(finding_ids):
        errors.append("unresolved review findings need nonempty unique IDs")
    for finding in unresolved:
        if not member(finding.get("severity"), {"blocking", "substantive", "minor"}) or not text(finding.get("defect")):
            errors.append("unresolved finding needs severity and concrete defect")
        elif finding["severity"] == "blocking" and run.get("completion_status") != "incomplete":
            errors.append("unresolved blocking factual/citation finding requires incomplete status")
        elif finding["severity"] == "substantive":
            limited("unresolved substantive review finding must be disclosed")
    deviations = rows(run, "deviations")
    if any(not text(d.get("rule")) or not text(d.get("reason")) for d in deviations):
        errors.append("each deviation needs a specific rule and reason")
    if not full and not deviations:
        errors.append("bounded/incomplete runs need specific deviations")
    return errors, warnings


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run", type=Path)
    parser.add_argument("evidence", type=Path)
    args = parser.parse_args()
    try:
        errors, warnings = audit_run(json.loads(args.run.read_text()), json.loads(args.evidence.read_text()))
    except (OSError, ValueError) as exc:
        parser.exit(2, f"Cannot read audit inputs: {exc}\n")
    for label, findings in (("ERROR", errors), ("WARN", warnings)):
        for finding in findings:
            print(f"{label}: {finding}")
    print(f"Run audit: {len(errors)} errors, {len(warnings)} warnings; recorded gates only.")
    return bool(errors)


if __name__ == "__main__":
    raise SystemExit(main())
