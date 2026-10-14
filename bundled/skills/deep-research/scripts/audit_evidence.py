#!/usr/bin/env python3
"""Audit a deep-research evidence ledger before report generation.

Architecture map:
    JSON file -> structural checks -> source checks -> claim checks -> readable findings

The script intentionally checks only facts that can be verified mechanically. It
cannot decide whether a study is trustworthy or whether a conclusion is sound.
Those judgments remain part of the research protocol.

Main callers:
    Bladerunner agents using the deep-research skill before and after drafting.

Why this exists:
    Claim-to-source gaps are easy to miss in a long report. A deterministic audit
    catches missing fields, dead references, weak support patterns, and common
    bookkeeping mistakes before they become polished-looking PDF errors.
"""

from __future__ import annotations

import argparse
import ipaddress
import json
import re
import sys
from dataclasses import dataclass
from datetime import date
from pathlib import Path
from typing import Any
from urllib.parse import unquote, urlparse


SOURCE_TYPES = {
    "systematic_review",
    "meta_analysis",
    "randomized_trial",
    "observational_study",
    "primary_research",
    "official_data",
    "regulation",
    "standard",
    "guideline",
    "technical_documentation",
    "preprint",
    "expert_opinion",
    "news",
    "vendor_content",
    "other",
}
CLAIM_IMPORTANCE = {"key", "supporting"}
CLAIM_STATUS = {"supported", "mixed", "uncertain", "unsupported"}
CONFIDENCE = {"high", "moderate", "low", "very_low"}
WEAK_SOURCE_TYPES = {"expert_opinion", "news", "vendor_content", "other"}
DATE_PATTERN = re.compile(r"^\d{4}(?:-\d{2}-\d{2})?$")


@dataclass(frozen=True)
class Finding:
    """Represent one audit result with a stable severity and location."""

    severity: str
    location: str
    message: str


class Audit:
    """Collect errors and warnings without hiding later findings after one failure."""

    def __init__(self) -> None:
        self.findings: list[Finding] = []

    def error(self, location: str, message: str) -> None:
        self.findings.append(Finding("ERROR", location, message))

    def warn(self, location: str, message: str) -> None:
        self.findings.append(Finding("WARN", location, message))


def is_nonempty_string(value: Any) -> bool:
    return isinstance(value, str) and bool(value.strip())


def require_object(audit: Audit, value: Any, location: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        audit.error(location, "must be a JSON object")
        return {}
    return value


def require_list(audit: Audit, value: Any, location: str) -> list[Any]:
    if not isinstance(value, list):
        audit.error(location, "must be a JSON array")
        return []
    return value


def require_text(audit: Audit, item: dict[str, Any], field: str, location: str) -> str:
    value = item.get(field)
    if not is_nonempty_string(value):
        audit.error(f"{location}.{field}", "must be a non-empty string")
        return ""
    return value.strip()


def validate_date(audit: Audit, value: str, location: str, allow_year: bool = False,
                  cutoff: date | None = None) -> None:
    if not DATE_PATTERN.fullmatch(value):
        audit.error(location, "must use YYYY-MM-DD" + (" or YYYY" if allow_year else ""))
        return
    if len(value) == 4 and allow_year:
        year = int(value)
        if year < 1:
            audit.error(location, "is not a valid calendar year")
        elif year > date.today().year:
            audit.warn(location, "is in the future")
        if cutoff and year > cutoff.year:
            audit.error(location, "is newer than the evidence cutoff")
        elif cutoff and year == cutoff.year and cutoff < date(year, 12, 31):
            audit.warn(location, "year-only date cannot establish publication before the evidence cutoff")
        return
    try:
        parsed = date.fromisoformat(value)
    except ValueError:
        audit.error(location, "is not a valid calendar date")
        return
    if parsed > date.today():
        audit.warn(location, "is in the future")
    if cutoff and parsed > cutoff:
        audit.error(location, "is newer than the evidence cutoff")


def valid_hostname(hostname: str) -> bool:
    if re.search(r"%(?![0-9a-fA-F]{2})", hostname):
        return False
    try:
        hostname = unquote(hostname, errors="strict")
        if any(c.isspace() for c in hostname):
            return False
        ipaddress.ip_address(hostname)
        return True
    except UnicodeError:
        return False
    except ValueError:
        pass
    try:
        name = hostname.encode("idna").decode("ascii").removesuffix(".")
    except UnicodeError:
        return False
    if re.fullmatch(r"[0-9]+(?:\.[0-9]+){3}", name):
        return False  # A dotted IPv4 literal must pass ip_address above.
    labels = name.split(".")
    return len(name) <= 253 and all(
        re.fullmatch(r"[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?", label)
        for label in labels
    )


def validate_url(audit: Audit, value: str, location: str) -> None:
    if any(c.isspace() or ord(c) <= 32 or 127 <= ord(c) <= 159 or c in '\\<>"^`{|}' for c in value) or re.search(r"%(?![0-9a-fA-F]{2})", value):
        audit.error(location, "must not contain forbidden URL characters or malformed percent escapes")
        return
    try:
        parsed = urlparse(value)
        hostname = parsed.hostname
        _ = parsed.port  # Access validates malformed and out-of-range ports.
    except ValueError:
        audit.error(location, "must be an absolute http(s) URL with a valid host")
        return
    if parsed.scheme not in {"http", "https"} or not hostname or not valid_hostname(hostname):
        audit.error(location, "must be an absolute http(s) URL with a valid host and port")


def validate_root(audit: Audit, data: dict[str, Any]) -> None:
    question = require_text(audit, data, "research_question", "root")
    if question and len(question) < 12:
        audit.warn("root.research_question", "is unusually short for a frozen research question")

    as_of_date = require_text(audit, data, "as_of_date", "root")
    if as_of_date:
        validate_date(audit, as_of_date, "root.as_of_date")

    protocol = require_object(audit, data.get("protocol"), "root.protocol")
    for field in ("scope", "confidence_rule"):
        require_text(audit, protocol, field, "root.protocol")
    for field in ("inclusion_rules", "exclusion_rules"):
        values = require_list(audit, protocol.get(field), f"root.protocol.{field}")
        if not values:
            audit.warn(f"root.protocol.{field}", "is empty")
        for index, value in enumerate(values):
            if not is_nonempty_string(value):
                audit.error(f"root.protocol.{field}[{index}]", "must be a non-empty string")

    for field in ("conflicts", "protocol_deviations", "limitations"):
        require_list(audit, data.get(field), f"root.{field}")
    if isinstance(data.get("limitations"), list):
        for index, value in enumerate(data["limitations"]):
            if not is_nonempty_string(value):
                audit.error(f"root.limitations[{index}]", "must be a non-empty string")


def validate_search_log(audit: Audit, data: dict[str, Any]) -> None:
    search_log = require_list(audit, data.get("search_log"), "root.search_log")
    for index, raw_entry in enumerate(search_log):
        location = f"root.search_log[{index}]"
        entry = require_object(audit, raw_entry, location)
        require_text(audit, entry, "query", location)
        require_text(audit, entry, "purpose", location)
        require_text(audit, entry, "useful_result", location)
        value = require_text(audit, entry, "date", location)
        if value:
            validate_date(audit, value, f"{location}.date")


def validate_sources(audit: Audit, data: dict[str, Any]) -> dict[str, dict[str, Any]]:
    try:
        cutoff_value = data.get("as_of_date", "")
        cutoff = date.fromisoformat(cutoff_value.strip() if isinstance(cutoff_value, str) else cutoff_value)
    except (TypeError, ValueError):
        cutoff = None  # validate_root reports malformed cutoffs.
    raw_sources = require_list(audit, data.get("sources"), "root.sources")
    if not raw_sources:
        audit.warn("root.sources", "contains no inspected sources; report must disclose the evidence gap")

    sources: dict[str, dict[str, Any]] = {}
    for index, raw_source in enumerate(raw_sources):
        location = f"root.sources[{index}]"
        source = require_object(audit, raw_source, location)
        source_id = require_text(audit, source, "id", location)
        if source_id in sources:
            audit.error(f"{location}.id", f"duplicates source id {source_id!r}")
        elif source_id:
            sources[source_id] = source

        for field in ("title", "publisher", "independence_group", "relevance", "limitations"):
            require_text(audit, source, field, location)

        if source.get("inspected") is not True:
            audit.error(f"{location}.inspected", "must be true for a source included in the evidence ledger")
        for field in ("inspected_by", "inspection_notes"):
            require_text(audit, source, field, location)

        url = require_text(audit, source, "url", location)
        if url:
            validate_url(audit, url, f"{location}.url")

        publication_date = source.get("publication_date")
        if "publication_date" not in source:
            audit.error(f"{location}.publication_date", "must be a date or null for an undated source")
        elif publication_date is not None and not is_nonempty_string(publication_date):
            audit.error(f"{location}.publication_date", "must be a date or null for an undated source")
        elif publication_date is not None:
            validate_date(audit, publication_date, f"{location}.publication_date", allow_year=True, cutoff=cutoff)
        accessed_date = require_text(audit, source, "accessed_date", location)
        if accessed_date:
            validate_date(audit, accessed_date, f"{location}.accessed_date")

        source_type = require_text(audit, source, "source_type", location)
        if source_type and source_type not in SOURCE_TYPES:
            audit.error(f"{location}.source_type", f"must be one of {sorted(SOURCE_TYPES)}")

        if not isinstance(source.get("primary"), bool):
            audit.error(f"{location}.primary", "must be true or false")
        peer_reviewed = source.get("peer_reviewed")
        if peer_reviewed is not None and not isinstance(peer_reviewed, bool):
            audit.error(f"{location}.peer_reviewed", "must be true, false, or null")

    return sources


def evidence_source_ids(audit: Audit, claim: dict[str, Any], field: str, location: str) -> list[str]:
    entries = require_list(audit, claim.get(field), f"{location}.{field}")
    source_ids: list[str] = []
    for index, raw_entry in enumerate(entries):
        evidence_location = f"{location}.{field}[{index}]"
        entry = require_object(audit, raw_entry, evidence_location)
        source_id = require_text(audit, entry, "source_id", evidence_location)
        locator = require_text(audit, entry, "locator", evidence_location)
        support = require_text(audit, entry, "support", evidence_location)
        if locator.lower() in {"n/a", "na", "none", "unknown"}:
            audit.warn(f"{evidence_location}.locator", "is not a precise locator")
        if support and len(support) < 12:
            audit.warn(f"{evidence_location}.support", "is too short to explain what the source shows")
        if source_id:
            source_ids.append(source_id)
    return source_ids


def validate_claims(
    audit: Audit,
    data: dict[str, Any],
    sources: dict[str, dict[str, Any]],
) -> None:
    raw_claims = require_list(audit, data.get("claims"), "root.claims")
    if not raw_claims:
        audit.error("root.claims", "must contain at least one claim")

    seen_ids: set[str] = set()
    key_claim_count = 0
    for index, raw_claim in enumerate(raw_claims):
        location = f"root.claims[{index}]"
        claim = require_object(audit, raw_claim, location)
        claim_id = require_text(audit, claim, "id", location)
        if claim_id in seen_ids:
            audit.error(f"{location}.id", f"duplicates claim id {claim_id!r}")
        seen_ids.add(claim_id)

        text = require_text(audit, claim, "text", location)
        importance = require_text(audit, claim, "importance", location)
        status = require_text(audit, claim, "status", location)
        confidence = require_text(audit, claim, "confidence", location)
        require_text(audit, claim, "confidence_reason", location)
        require_text(audit, claim, "notes", location)

        if importance not in CLAIM_IMPORTANCE:
            audit.error(f"{location}.importance", f"must be one of {sorted(CLAIM_IMPORTANCE)}")
        if status not in CLAIM_STATUS:
            audit.error(f"{location}.status", f"must be one of {sorted(CLAIM_STATUS)}")
        if confidence not in CONFIDENCE:
            audit.error(f"{location}.confidence", f"must be one of {sorted(CONFIDENCE)}")
        if importance == "key":
            key_claim_count += 1

        supporting_ids = evidence_source_ids(audit, claim, "evidence", location)
        counter_ids = evidence_source_ids(audit, claim, "counterevidence", location)
        all_ids = supporting_ids + counter_ids
        for source_id in all_ids:
            if source_id not in sources:
                audit.error(location, f"references unknown source id {source_id!r}")

        if status in {"supported", "mixed"} and not supporting_ids:
            audit.error(f"{location}.evidence", "a supported or mixed claim must have supporting evidence")
        if status == "mixed" and not counter_ids:
            audit.warn(f"{location}.counterevidence", "a mixed claim should record counterevidence")
        if importance == "key" and status in {"uncertain", "unsupported"} and confidence in {"high", "moderate"}:
            audit.error(location, "an uncertain or unsupported key claim cannot have high or moderate confidence")

        valid_supporting = [sources[source_id] for source_id in supporting_ids if source_id in sources]
        source_types = {source.get("source_type") for source in valid_supporting
                        if is_nonempty_string(source.get("source_type"))}
        if importance == "key" and status == "supported" and source_types and source_types <= WEAK_SOURCE_TYPES:
            audit.warn(location, "a key claim relies on contextual source types; verify their fitness for this claim")

        if text and re.search(r"\d", text) and supporting_ids:
            notes = claim.get("notes", "")
            if not is_nonempty_string(notes) or len(notes.strip()) < 8:
                audit.warn(f"{location}.notes", "a quantitative claim should record units or calculation details")

    if raw_claims and key_claim_count == 0:
        audit.error("root.claims", "must contain at least one key claim")


def validate_conflicts(audit: Audit, data: dict[str, Any], sources: dict[str, dict[str, Any]]) -> None:
    conflicts = require_list(audit, data.get("conflicts"), "root.conflicts")
    for index, raw_conflict in enumerate(conflicts):
        location = f"root.conflicts[{index}]"
        conflict = require_object(audit, raw_conflict, location)
        require_text(audit, conflict, "topic", location)
        require_text(audit, conflict, "resolution", location)
        source_ids = require_list(audit, conflict.get("source_ids"), f"{location}.source_ids")
        valid_ids = [source_id for source_id in source_ids if is_nonempty_string(source_id)]
        distinct_ids = set(valid_ids)
        if len(distinct_ids) != len(valid_ids):
            audit.error(f"{location}.source_ids", "must not contain duplicate source IDs")
        if len(distinct_ids) < 2:
            audit.warn(f"{location}.source_ids", "a conflict normally involves at least two sources")
        for source_id in source_ids:
            if not is_nonempty_string(source_id):
                audit.error(f"{location}.source_ids", "all source ids must be non-empty strings")
            elif source_id not in sources:
                audit.error(f"{location}.source_ids", f"references unknown source id {source_id!r}")


def run_audit(data: Any) -> list[Finding]:
    """Run every audit stage and return all findings in deterministic order.

    Main callers:
        `main`, plus lightweight unit or integration checks.
    """

    audit = Audit()
    root = require_object(audit, data, "root")
    if not isinstance(data, dict):
        return audit.findings

    validate_root(audit, root)
    validate_search_log(audit, root)
    sources = validate_sources(audit, root)
    validate_claims(audit, root, sources)
    validate_conflicts(audit, root, sources)
    return audit.findings


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Audit a deep-research evidence ledger.")
    parser.add_argument("ledger", type=Path, help="Path to evidence.json")
    parser.add_argument(
        "--strict",
        action="store_true",
        help="Return a failing exit status for warnings as well as errors.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        data = json.loads(args.ledger.read_text(encoding="utf-8"))
    except FileNotFoundError:
        print(f"ERROR {args.ledger}: file does not exist", file=sys.stderr)
        return 2
    except (OSError, UnicodeError) as exc:
        print(f"ERROR {args.ledger}: could not read file: {exc}", file=sys.stderr)
        return 2
    except json.JSONDecodeError as exc:
        print(f"ERROR {args.ledger}:{exc.lineno}:{exc.colno}: invalid JSON: {exc.msg}", file=sys.stderr)
        return 2

    findings = run_audit(data)
    for finding in findings:
        print(f"{finding.severity} {finding.location}: {finding.message}")

    errors = sum(finding.severity == "ERROR" for finding in findings)
    warnings = sum(finding.severity == "WARN" for finding in findings)
    print(f"Audit complete: {errors} error(s), {warnings} warning(s)")

    if errors or (args.strict and warnings):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
