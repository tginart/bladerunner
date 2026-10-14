"""Behavioral checks for evidence bookkeeping; no network or model calls."""

import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from audit_evidence import run_audit


def ledger():
    source = {
        "id": "S1", "title": "Primary measurement", "url": "https://example.org/study",
        "publisher": "Research institution", "publication_date": "2025-01-01",
        "accessed_date": "2025-01-02", "source_type": "primary_research",
        "primary": True, "peer_reviewed": True, "independence_group": "study-A",
        "relevance": "Direct measurement of the scoped outcome",
        "limitations": "Results apply to the tested setting",
        "inspected": True, "inspected_by": "A1",
        "inspection_notes": "Read the methods, results, and uncertainty interval",
    }
    second = dict(source, id="S2", url="https://example.org/replication", independence_group="study-B")
    return {
        "research_question": "Does the intervention improve the measured outcome?",
        "as_of_date": "2025-01-02",
        "protocol": {"scope": "The tested intervention and setting",
                     "confidence_rule": "Appraise directness and independent replication",
                     "inclusion_rules": ["Direct measurements"], "exclusion_rules": ["Marketing claims"]},
        "search_log": [{"query": q, "assignment_id": "A1", "date": "2025-01-02", "purpose": purpose, "useful_result": "S1"}
                       for q, purpose in [("original study", "core"), ("replication", "replication"),
                                          ("null results", "contradiction")]],
        "sources": [source, second],
        "claims": [{"id": "C1", "text": "The intervention improves the measured outcome",
                    "importance": "key", "status": "supported", "confidence": "high",
                    "confidence_reason": "Two independent direct studies agree",
                    "evidence": [{"source_id": s, "locator": "Results, Table 1",
                                  "support": "Direct outcome measurement"} for s in ["S1", "S2"]],
                    "counterevidence": [], "notes": "Applies only to the measured setting"}],
        "conflicts": [], "protocol_deviations": [], "limitations": ["Only one tested setting"],
    }


class EvidenceAuditTests(unittest.TestCase):
    def test_complete_independent_evidence_passes(self):
        self.assertEqual(run_audit(ledger()), [])

    def test_dangling_reference_is_rejected(self):
        data = ledger()
        data["claims"][0]["evidence"][0]["source_id"] = "missing"
        self.assertTrue(any(f.severity == "ERROR" and "unknown source" in f.message for f in run_audit(data)))

    def test_overall_limitations_are_nonempty_text(self):
        for value in [None, {}, [], 0, "", " "]:
            with self.subTest(value=value):
                data = ledger()
                data["limitations"] = [value]
                self.assertTrue(any(f.severity == "ERROR" and f.location == "root.limitations[0]" for f in run_audit(data)))
        data = ledger()
        data["limitations"] = []
        self.assertEqual(run_audit(data), [])

    def test_conflicts_require_distinct_source_ids(self):
        data = ledger()
        conflict = {"topic": "Difference in reported measurements", "source_ids": ["S1", "S1"],
                    "resolution": "Unresolved discrepancy limits confidence"}
        data["conflicts"] = [conflict]
        findings = run_audit(data)
        self.assertTrue(any(f.severity == "ERROR" and "duplicate" in f.message for f in findings))
        self.assertTrue(any(f.severity == "WARN" and "two sources" in f.message for f in findings))
        conflict["source_ids"] = ["S1", "S2"]
        self.assertEqual(run_audit(data), [])
        conflict["source_ids"] = ["S1", {}]
        self.assertTrue(any(f.severity == "ERROR" for f in run_audit(data)))

    def test_narrow_official_fact_needs_no_source_or_query_quorum(self):
        data = ledger()
        data["sources"] = data["sources"][:1]
        data["sources"][0].update(source_type="official_data", peer_reviewed=None)
        data["search_log"] = data["search_log"][:1]
        claim = data["claims"][0]
        claim.update(text="The agency reports this scoped measurement",
                     confidence_reason="Directly verified in the current authoritative table")
        claim["evidence"] = claim["evidence"][:1]
        self.assertEqual(run_audit(data), [])

    def test_uninspected_source_is_rejected(self):
        data = ledger()
        data["sources"][0]["inspected"] = False
        self.assertTrue(any(f.severity == "ERROR" and f.location.endswith(".inspected") for f in run_audit(data)))

    def test_missing_locator_is_rejected(self):
        data = ledger()
        del data["claims"][0]["evidence"][0]["locator"]
        self.assertTrue(any(f.severity == "ERROR" and f.location.endswith(".locator") for f in run_audit(data)))

    def test_undated_source_does_not_require_inventing_a_date(self):
        data = ledger()
        data["sources"][0]["publication_date"] = None
        self.assertEqual(run_audit(data), [])

    def test_year_only_dates_validate_calendar_and_future_evidence(self):
        for value, severity in [("0000", "ERROR"), ("9999", "WARN")]:
            with self.subTest(year=value):
                data = ledger()
                data["sources"][0]["publication_date"] = value
                self.assertTrue(any(f.severity == severity and f.location.endswith("publication_date") for f in run_audit(data)))
        data = ledger()
        data["as_of_date"] = "2025-12-31"
        data["sources"][0]["publication_date"] = "2025"
        self.assertEqual(run_audit(data), [])

    def test_publication_dates_respect_historical_cutoff(self):
        for cutoff in ["2024-01-01", " 2024-01-01 "]:
            for publication in ["2025-01-01", "2025", "2024-01-02"]:
                with self.subTest(cutoff=cutoff, publication=publication):
                    data = ledger()
                    data["as_of_date"] = cutoff
                    data["sources"][0]["publication_date"] = publication
                    self.assertTrue(any(f.severity == "ERROR" and "evidence cutoff" in f.message for f in run_audit(data)))
        data = ledger()
        for publication in ["2025-01-02", "2024-12-31", "2024", None]:
            with self.subTest(publication=publication):
                data["sources"][0]["publication_date"] = publication
                self.assertEqual(run_audit(data), [])

    def test_year_only_date_discloses_cutoff_uncertainty(self):
        data = ledger()
        data["sources"][0]["publication_date"] = "2025"
        self.assertTrue(any(f.severity == "WARN" and "year-only" in f.message for f in run_audit(data)))

    def test_urls_require_a_hostname_and_valid_port(self):
        for url in ["https://user@", "https://:443", "https://example.org:bad",
                    "https://example.org:65536", "https://bad host/"]:
            with self.subTest(url=url):
                data = ledger()
                data["sources"][0]["url"] = url
                self.assertTrue(any(f.severity == "ERROR" and f.location.endswith(".url") for f in run_audit(data)))
        for url in ["https://example.org:443/path", "https://[::1]:443/path", "https://user@example.org/path"]:
            with self.subTest(url=url):
                data = ledger()
                data["sources"][0]["url"] = url
                self.assertEqual(run_audit(data), [])

    def test_entire_url_rejects_forbidden_characters_and_bad_escapes(self):
        for remainder in ["/bad path", "/%zz", "/percent%", "/bad\\path",
                          "/tab\tpath", "/line\npath", "/null\x00path", "/delete\x7fpath",
                          "/control\x80path", "/unicode\u00a0space",
                          '/"quoted"', "/<tag>", "/{value}", "/pipe|value", "/caret^", "/tick`",
                          "/path?query=%zz", "/path#fragment%zz"]:
            with self.subTest(remainder=remainder):
                data = ledger()
                data["sources"][0]["url"] = "https://example.org" + remainder
                self.assertTrue(any(f.severity == "ERROR" and f.location.endswith(".url") for f in run_audit(data)))
        for remainder in ["/a%20path", "/path?q=a%2Bb#part%20one", "/данные"]:
            with self.subTest(remainder=remainder):
                data = ledger()
                data["sources"][0]["url"] = "https://example.org" + remainder
                self.assertEqual(run_audit(data), [])

    def test_invalid_host_is_reported_without_hiding_later_findings(self):
        data = ledger()
        data["sources"][0]["url"] = "http://["
        data["claims"][0]["evidence"][1]["source_id"] = "missing"
        findings = run_audit(data)
        self.assertTrue(any(f.severity == "ERROR" and f.location.endswith(".url") for f in findings))
        self.assertTrue(any(f.severity == "ERROR" and "unknown source" in f.message for f in findings))

    def test_hostname_syntax_preserves_ip_dns_and_international_names(self):
        invalid = ["%", ".", "example..com", "example.org..", "-bad.example",
                   "bad-.example", "bad_name.example", "999.999.999.999",
                   "a" * 64 + ".example", ".".join(["a" * 63] * 4)]
        valid = ["localhost", "1host.example", "example.org.", "127.0.0.1",
                 "[2001:db8::1]", "bücher.example", "xn--bcher-kva.example",
                 "%65xample.org", ".".join(["a" * 63] * 3 + ["a" * 61])]
        for hostname in invalid:
            with self.subTest(hostname=hostname):
                data = ledger()
                data["sources"][0]["url"] = "https://" + hostname
                self.assertTrue(any(f.severity == "ERROR" and f.location.endswith(".url") for f in run_audit(data)))
        for hostname in valid:
            with self.subTest(hostname=hostname):
                data = ledger()
                data["sources"][0]["url"] = "https://" + hostname
                self.assertEqual(run_audit(data), [])

    def test_mixed_claim_needs_support_not_just_counterevidence(self):
        data = ledger()
        claim = data["claims"][0]
        claim.update(status="mixed", counterevidence=claim["evidence"][:1], evidence=[])
        self.assertTrue(any(f.severity == "ERROR" and f.location.endswith(".evidence") for f in run_audit(data)))
        claim["evidence"] = [{"source_id": "S2", "locator": "Results, Table 1", "support": "Direct outcome measurement"}]
        self.assertEqual(run_audit(data), [])

    def test_cli_invalid_host_exits_with_findings_instead_of_traceback(self):
        script = Path(__file__).with_name("audit_evidence.py")
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary)/"evidence.json"
            data = ledger()
            data["sources"][0]["url"] = "http://["
            path.write_text(json.dumps(data))
            result = subprocess.run([sys.executable, str(script), str(path)], capture_output=True)
            self.assertEqual(result.returncode, 1)
            self.assertIn(b"root.sources[0].url", result.stdout)
            self.assertNotIn(b"Traceback", result.stderr)

    def test_inconclusive_report_can_disclose_no_sources(self):
        data = ledger()
        data["sources"] = []
        claim = data["claims"][0]
        claim.update(status="uncertain", confidence="very_low", evidence=[],
                     confidence_reason="Retrieval was unavailable")
        findings = run_audit(data)
        self.assertFalse(any(f.severity == "ERROR" for f in findings))
        self.assertTrue(any("no inspected sources" in f.message for f in findings))

    def test_cli_strict_fails_on_warning_but_accepts_valid_evidence(self):
        script = Path(__file__).with_name("audit_evidence.py")
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary)/"evidence.json"
            data = ledger()
            path.write_text(json.dumps(data))
            valid = subprocess.run([sys.executable, str(script), str(path), "--strict"], capture_output=True)
            self.assertEqual(valid.returncode, 0)
            data = copy.deepcopy(data)
            data["claims"][0]["status"] = "mixed"
            path.write_text(json.dumps(data))
            strict = subprocess.run([sys.executable, str(script), str(path), "--strict"], capture_output=True)
            ordinary = subprocess.run([sys.executable, str(script), str(path)], capture_output=True)
            self.assertEqual(strict.returncode, 1)
            self.assertEqual(ordinary.returncode, 0)


if __name__ == "__main__":
    unittest.main()
