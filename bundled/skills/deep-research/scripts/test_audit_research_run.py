"""Offline adaptive-run checks; these do not evaluate live research quality."""

import json
import os
import shutil
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from audit_research_run import audit_run, STAGES
from audit_evidence import run_audit
from test_audit_evidence import ledger


def fixture():
    run = {
        "schema_version": 2, "completion_status": "full", "stop_reason": "sufficient_evidence",
        "limits": {"max_assignments": None, "max_research_rounds": None, "max_sources": None},
        "subquestions": [{"id": "Q1", "question": "What is the scoped measurement?"}],
        "lead_question_ids": [],
        "assignments": [{"id": "A1", "task_id": "runtime-A1", "role": "researcher", "round": 1,
                         "status": "completed", "question_ids": ["Q1"], "tool_call_budget": 10,
                         "tool_calls_used": None},
                        {"id": "CV", "task_id": "runtime-CV", "role": "citation_verifier", "round": 1,
                         "status": "completed", "question_ids": []}],
        "coverage": [{"question_id": "Q1", "claim_ids": ["C1"], "gap": ""}],
        "research_decisions": [{"after_round": 1, "action": "synthesize", "reason": "Scoped fact verified"}],
        "stages": {name: {"status": "completed"} for name in STAGES},
        "artifacts": {"markdown": True, "evidence_ledger": True, "pdf_generated": False,
                      "pdf_visual_verified": False},
        "required_artifacts": ["markdown", "evidence_ledger"],
        "deviations": [], "unresolved_review_findings": [],
    }
    run["stages"]["citations"]["performed_by"] = ["CV"]
    evidence = {"claims": [{"id": "C1"}],
                "sources": [{"id": "S1", "url": "https://example.org/official-data", "primary": True,
                             "inspected": True, "inspected_by": "A1"}],
                "search_log": [{"assignment_id": "A1", "query": "official measurement"}]}
    return run, evidence


def add_researcher(run, number, round_number=1, status="completed"):
    run["assignments"].insert(-1, {"id": f"A{number}", "task_id": f"runtime-A{number}",
                                 "role": "researcher", "round": round_number, "status": status,
                                 "question_ids": ["Q1"]})


class RunAuditTests(unittest.TestCase):
    def test_one_researcher_one_source_no_pdf_can_be_full(self):
        self.assertEqual(audit_run(*fixture()), ([], []))

    def test_evidence_and_run_audits_accept_consistent_records(self):
        run, _ = fixture()
        evidence = ledger()
        self.assertEqual(run_audit(evidence), [])
        self.assertEqual(audit_run(run, evidence), ([], []))

    def test_large_team_not_limited_to_four(self):
        run, evidence = fixture()
        for i in range(2, 13):
            add_researcher(run, i)
        self.assertEqual(audit_run(run, evidence), ([], []))

    def test_three_research_rounds_allowed_without_user_cap(self):
        run, evidence = fixture()
        for n in (2, 3):
            add_researcher(run, n, n)
        run["research_decisions"] = [
            {"after_round": n, "action": "follow_up" if n < 3 else "synthesize",
             "reason": "Investigate unresolved methods" if n < 3 else "Methods now verified"} for n in (1, 2, 3)]
        self.assertEqual(audit_run(run, evidence), ([], []))

    def test_more_than_twenty_needs_reason_but_is_not_hard_cap(self):
        run, evidence = fixture()
        for i in range(2, 22):
            add_researcher(run, i)
        self.assertTrue(any("team_expansion_reason" in x for x in audit_run(run, evidence)[0]))
        run["team_expansion_reason"] = "21 independent jurisdictions require separate official-source retrieval"
        self.assertEqual(audit_run(run, evidence), ([], []))

    def test_failed_retry_and_verifier_consume_assignment_cap(self):
        run, evidence = fixture()
        run["limits"]["max_assignments"] = 2
        add_researcher(run, 2, status="failed")
        self.assertTrue(any("assignment cap" in x for x in audit_run(run, evidence)[0]))

    def test_explicit_round_cap_respected(self):
        run, evidence = fixture()
        run["limits"]["max_research_rounds"] = 1
        add_researcher(run, 2, 2)
        self.assertTrue(any("round cap" in x for x in audit_run(run, evidence)[0]))

    def test_global_source_cap_checked(self):
        run, evidence = fixture()
        run["limits"]["max_sources"] = 1
        evidence["sources"].append(dict(evidence["sources"][0], id="S2", url="https://example.org/second"))
        self.assertTrue(any("source cap" in x for x in audit_run(run, evidence)[0]))
        evidence["sources"][1]["url"] = evidence["sources"][0]["url"] + "#table"
        self.assertEqual(audit_run(run, evidence), ([], []))

    def test_observed_worker_budget_overrun_rejected(self):
        run, evidence = fixture()
        run["assignments"][0]["tool_calls_used"] = 11
        self.assertTrue(any("tool-call budget" in x for x in audit_run(run, evidence)[0]))

    def test_optional_doi_values_cannot_bypass_source_cap(self):
        for doi in ["unknown", "10.1234/example", [], None]:
            with self.subTest(doi=doi):
                run, evidence = fixture()
                run["limits"]["max_sources"] = 1
                evidence["sources"][0]["doi"] = doi
                evidence["sources"].append(dict(evidence["sources"][0], id="S2", url="https://example.org/second"))
                self.assertTrue(any("source cap" in x for x in audit_run(run, evidence)[0]))
                evidence["sources"][1]["url"] = evidence["sources"][0]["url"] + "#results"
                self.assertEqual(audit_run(run, evidence), ([], []))

    def test_decisions_follow_round_order_and_synthesize_last_round(self):
        run, evidence = fixture()
        add_researcher(run, 2, 2)
        run["research_decisions"] = [
            {"after_round": 2, "action": "follow_up", "reason": "Inspect newer evidence"},
            {"after_round": 1, "action": "synthesize", "reason": "Finish report"},
        ]
        errors, _ = audit_run(run, evidence)
        self.assertTrue(any("round order" in x for x in errors))
        self.assertTrue(any("last research round" in x for x in errors))
        run["research_decisions"] = [
            {"after_round": 1, "action": "follow_up", "reason": "Inspect newer evidence"},
            {"after_round": 2, "action": "follow_up", "reason": "Check final results"},
            {"after_round": 2, "action": "synthesize", "reason": "Finish report"},
        ]
        self.assertEqual(audit_run(run, evidence), ([], []))

    def test_lead_only_decision_uses_round_zero(self):
        run, evidence = fixture()
        run["assignments"].pop(0)
        run["lead_question_ids"] = ["Q1"]
        run["research_decisions"][0]["after_round"] = 0
        evidence["sources"][0]["inspected_by"] = "LEAD"
        evidence["search_log"][0]["assignment_id"] = "LEAD"
        self.assertEqual(audit_run(run, evidence), ([], []))

    def test_malformed_source_url_collects_error_and_continues(self):
        run, evidence = fixture()
        evidence["sources"][0]["url"] = "http://[#table"
        run["research_decisions"] = []
        errors, _ = audit_run(run, evidence)
        self.assertTrue(any("malformed source URL" in x for x in errors))
        self.assertTrue(any("decision" in x for x in errors))

    def test_unknown_claim_and_inspector_rejected(self):
        run, evidence = fixture()
        run["coverage"][0]["claim_ids"] = ["missing"]
        evidence["sources"][0]["inspected_by"] = "invented"
        self.assertGreaterEqual(len(audit_run(run, evidence)[0]), 2)

    def test_multiple_researchers_can_cover_same_question(self):
        run, evidence = fixture()
        add_researcher(run, 2)
        self.assertEqual(audit_run(run, evidence), ([], []))

    def test_unanswered_coverage_cannot_be_full(self):
        run, evidence = fixture()
        run["coverage"][0].update(claim_ids=[], gap="No applicable measurements retrieved")
        self.assertTrue(any("unanswered coverage limits" in x for x in audit_run(run, evidence)[0]))
        for status in ["bounded", "incomplete"]:
            with self.subTest(status=status):
                run.update(completion_status=status, stop_reason="access")
                run["deviations"] = [{"rule": "coverage", "reason": "No applicable measurements retrieved"}]
                errors, warnings = audit_run(run, evidence)
                self.assertEqual(errors, [])
                self.assertTrue(any("unanswered coverage limits" in x for x in warnings))
        run.update(completion_status="full", stop_reason="sufficient_evidence", deviations=[])
        run["coverage"][0].update(claim_ids=["C1"], gap="")
        evidence["claims"][0]["status"] = "uncertain"
        self.assertEqual(audit_run(run, evidence), ([], []))

    def test_bounded_status_requires_an_actual_limiting_stop_reason(self):
        for reason in ["sufficient_evidence", "diminishing_returns", "budget", "access", "tool_failure"]:
            with self.subTest(reason=reason):
                run, evidence = fixture()
                run.update(completion_status="bounded", stop_reason=reason)
                run["deviations"] = [{"rule": "comparison scope", "reason": "Limit reduced comparison coverage"}]
                errors, _ = audit_run(run, evidence)
                if reason in {"budget", "access", "tool_failure"}:
                    self.assertEqual(errors, [])
                else:
                    self.assertTrue(any("bounded status needs" in x for x in errors))

    def test_source_counting_rejects_malformed_url_remainders(self):
        for url in ["https://example.org/bad path", "https://example.org/%zz", "https://example.org/bad\\path"]:
            with self.subTest(url=url):
                run, evidence = fixture()
                evidence["sources"][0]["url"] = url
                self.assertTrue(any("malformed source URL" in x for x in audit_run(run, evidence)[0]))

    def test_source_caps_normalize_equivalent_urls_and_preserve_versions(self):
        equivalent_pairs = [
            ("https://example.org/data", "HTTPS://EXAMPLE.ORG:443/data#table"),
            ("http://example.org/data", "http://EXAMPLE.ORG:80/data"),
            ("https://example.org", "https://example.org/"),
            ("https://example.org/data", "https://example.org./data"),
            ("https://bücher.example/data", "https://xn--bcher-kva.example/data"),
            ("https://example.org/data", "https://%65xample.org/data"),
            ("https://[2001:db8::1]/data", "https://[2001:DB8:0:0:0:0:0:1]:443/data"),
        ]
        distinct_pairs = [
            ("https://example.org/data", "https://example.org/Data"),
            ("https://example.org/data", "https://example.org/data/"),
            ("https://example.org/data?v=1", "https://example.org/data?v=2"),
            ("https://example.org/data", "https://example.org:444/data"),
            ("https://example.org/data", "http://example.org/data"),
            ("https://user@example.org/data", "https://other@example.org/data"),
            ("https://[fe80::1%25en0]/data", "https://[fe80::1%25EN0]/data"),
        ]
        for pairs, should_pass in [(equivalent_pairs, True), (distinct_pairs, False)]:
            for first, second in pairs:
                with self.subTest(first=first, second=second):
                    run, evidence = fixture()
                    run["limits"]["max_sources"] = 1
                    evidence["sources"][0]["url"] = first
                    evidence["sources"].append(dict(evidence["sources"][0], id="S2", url=second))
                    errors, _ = audit_run(run, evidence)
                    self.assertEqual(errors == [], should_pass)
                    if not should_pass:
                        self.assertTrue(any("source cap" in x for x in errors))

    def test_lead_only_citation_check_needs_disclosure(self):
        run, evidence = fixture()
        run["assignments"].pop()
        run["stages"]["citations"]["performed_by"] = ["LEAD"]
        self.assertTrue(any("substitution_reason" in x for x in audit_run(run, evidence)[0]))
        run["stages"]["citations"]["substitution_reason"] = "User assignment cap reserves lead verification"
        self.assertEqual(audit_run(run, evidence), ([], []))

    def test_failed_verifier_cannot_count_as_completed_check(self):
        run, evidence = fixture()
        run["assignments"][-1]["status"] = "failed"
        self.assertTrue(any("citation performer" in x for x in audit_run(run, evidence)[0]))

    def test_distinct_review_roles_cannot_impersonate_citation_verifier(self):
        for role in ["report_reviewer", "evidence_auditor", "researcher"]:
            with self.subTest(role=role):
                run, evidence = fixture()
                run["assignments"][-1]["role"] = role
                self.assertTrue(any("citation performer" in x for x in audit_run(run, evidence)[0]))

    def test_blocking_findings_require_incomplete(self):
        run, evidence = fixture()
        run["completion_status"] = "bounded"
        run["stop_reason"] = "access"
        run["deviations"] = [{"rule": "pivotal fact", "reason": "Citation could not be verified"}]
        run["unresolved_review_findings"] = [{"id": "CV1", "severity": "blocking", "defect": "Central citation incorrect"}]
        self.assertTrue(any("requires incomplete" in x for x in audit_run(run, evidence)[0]))
        run["completion_status"] = "incomplete"
        self.assertFalse(audit_run(run, evidence)[0])

    def test_unresolved_findings_need_nonempty_unique_ids(self):
        for identifier in [None, "", " ", [], {}]:
            with self.subTest(identifier=identifier):
                run, evidence = fixture()
                run["unresolved_review_findings"] = [{"id": identifier, "severity": "minor", "defect": "Clarify a sentence"}]
                self.assertTrue(any("nonempty unique IDs" in x for x in audit_run(run, evidence)[0]))
        run, evidence = fixture()
        run["unresolved_review_findings"] = [{"severity": "minor", "defect": "Clarify a sentence"}]
        self.assertTrue(any("nonempty unique IDs" in x for x in audit_run(run, evidence)[0]))
        finding = run["unresolved_review_findings"][0]
        finding["id"] = "F1"
        self.assertEqual(audit_run(run, evidence), ([], []))
        run["unresolved_review_findings"].append(dict(finding, id=" F1 "))
        self.assertTrue(any("nonempty unique IDs" in x for x in audit_run(run, evidence)[0]))
        run["unresolved_review_findings"][-1]["id"] = "F2"
        self.assertEqual(audit_run(run, evidence), ([], []))

    @unittest.skipIf(os.name == "nt", "POSIX shell example is tested on Unix runners")
    def test_documented_shell_gate_preserves_each_auditor_failure(self):
        skill_dir = Path(__file__).resolve().parent.parent
        skill = (skill_dir/"SKILL.md").read_text()
        commands = skill.split("```bash\n", 1)[1].split("```", 1)[0]
        for bad_evidence, bad_run in [(False, False), (True, False), (False, True), (True, True)]:
            with self.subTest(evidence_failure=bad_evidence, run_failure=bad_run), tempfile.TemporaryDirectory() as temporary:
                run, _ = fixture()
                evidence = ledger()
                if bad_evidence:
                    evidence["sources"][0]["publication_date"] = "0000"
                if bad_run:
                    run["schema_version"] = 1
                tmp = Path(temporary)
                (tmp/"run.json").write_text(json.dumps(run))
                (tmp/"evidence.json").write_text(json.dumps(evidence))
                result = subprocess.run(["bash", "-e", "-c", commands], capture_output=True,
                                        env=dict(os.environ, SKILL_DIR=str(skill_dir), research_output_dir=temporary))
                self.assertEqual(result.returncode, int(bad_evidence or bad_run))
                self.assertIn(b"Audit complete:", result.stdout)
                self.assertIn(b"Run audit:", result.stdout)
                self.assertNotIn(b"Traceback", result.stderr)

    @unittest.skipUnless(os.name == "nt", "Windows PowerShell example is tested on the Windows CI runner")
    def test_documented_powershell_gate_preserves_each_auditor_failure(self):
        shell = shutil.which("powershell") or shutil.which("pwsh")
        self.assertIsNotNone(shell, "Windows CI must exercise the documented PowerShell gate")
        skill_dir = Path(__file__).resolve().parent.parent
        skill = (skill_dir/"SKILL.md").read_text()
        commands = skill.split("```powershell\n", 1)[1].split("```", 1)[0]
        for bad_evidence, bad_run in [(False, False), (True, False), (False, True), (True, True)]:
            with self.subTest(evidence_failure=bad_evidence, run_failure=bad_run), tempfile.TemporaryDirectory(prefix="research audit ") as temporary:
                run, _ = fixture()
                evidence = ledger()
                if bad_evidence:
                    evidence["sources"][0]["publication_date"] = "0000"
                if bad_run:
                    run["schema_version"] = 1
                tmp = Path(temporary)
                (tmp/"run.json").write_text(json.dumps(run))
                (tmp/"evidence.json").write_text(json.dumps(evidence))
                preamble = "$SKILL_DIR = '" + str(skill_dir).replace("'", "''") + "'\n"
                preamble += "$research_output_dir = '" + temporary.replace("'", "''") + "'\n"
                script = tmp/"audit-gate.ps1"
                script.write_text(preamble + commands, encoding="utf-8-sig")
                result = subprocess.run([shell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(script)], capture_output=True)
                self.assertEqual(result.returncode, int(bad_evidence or bad_run), result.stdout + result.stderr)
                self.assertIn(b"Audit complete:", result.stdout)
                self.assertIn(b"Run audit:", result.stdout)
                self.assertNotIn(b"Traceback", result.stderr)

    def test_required_pdf_cannot_be_bounded(self):
        run, evidence = fixture()
        run["required_artifacts"].append("pdf")
        self.assertTrue(any("requires incomplete" in x for x in audit_run(run, evidence)[0]))
        run["completion_status"] = "incomplete"
        run["deviations"] = [{"rule": "PDF", "reason": "No PDF tool available"}]
        self.assertFalse(audit_run(run, evidence)[0])

    def test_uninspected_delivered_pdf_not_full(self):
        run, evidence = fixture()
        run["artifacts"]["pdf_generated"] = True
        self.assertTrue(any("visual verification" in x for x in audit_run(run, evidence)[0]))
        run["artifacts"]["pdf_visual_verified"] = True
        self.assertFalse(audit_run(run, evidence)[0])

    def test_access_limited_run_discloses_missing_dimension(self):
        run, evidence = fixture()
        run.update(completion_status="bounded", stop_reason="access")
        self.assertTrue(any("specific deviations" in x for x in audit_run(run, evidence)[0]))
        run["deviations"] = [{"rule": "comparison scope", "reason": "Comparator data unavailable; cannot rank alternatives"}]
        self.assertFalse(audit_run(run, evidence)[0])
        run["completion_status"] = "full"
        self.assertTrue(any("full cannot hide" in x for x in audit_run(run, evidence)[0]))

    def test_skipped_work_cannot_supply_evidence(self):
        run, evidence = fixture()
        run["assignments"][0].update(status="skipped", reason="No access")
        del run["assignments"][0]["task_id"]
        self.assertTrue(any("skipped assignment cannot" in x for x in audit_run(run, evidence)[0]))

    def test_missing_decision_and_uncollected_results_rejected(self):
        run, evidence = fixture()
        run["research_decisions"] = []
        run["assignments"][0]["status"] = "running"
        errors, _ = audit_run(run, evidence)
        self.assertTrue(any("running work" in x for x in errors))
        self.assertTrue(any("research decision" in x for x in errors))

    def test_malformed_fields_return_errors(self):
        for field in ["completion_status", "stop_reason", "role", "status"]:
            run, evidence = fixture()
            (run if field in {"completion_status", "stop_reason"} else run["assignments"][0])[field] = []
            self.assertTrue(audit_run(run, evidence)[0])
        self.assertTrue(audit_run(None, {})[0])
        self.assertTrue(audit_run({}, {})[0])

    def test_cli_reports_valid_record_and_bad_input(self):
        script = Path(__file__).with_name("audit_research_run.py")
        with tempfile.TemporaryDirectory() as temporary:
            tmp = Path(temporary)
            run, evidence = fixture()
            (tmp/"run.json").write_text(json.dumps(run))
            (tmp/"evidence.json").write_text(json.dumps(evidence))
            good = subprocess.run([sys.executable, str(script), str(tmp/"run.json"), str(tmp/"evidence.json")], capture_output=True)
            self.assertEqual(good.returncode, 0)
            run["assignments"][0]["role"] = {}
            (tmp/"run.json").write_text(json.dumps(run))
            bad = subprocess.run([sys.executable, str(script), str(tmp/"run.json"), str(tmp/"evidence.json")], capture_output=True)
            self.assertEqual(bad.returncode, 1)
            self.assertNotIn(b"Traceback", bad.stderr)


if __name__ == "__main__":
    unittest.main()
