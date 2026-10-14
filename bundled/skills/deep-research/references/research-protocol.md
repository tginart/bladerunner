# Research protocol and evidence ledger

## Contents

1. Question framing
2. Search design
3. Source selection
4. Critical appraisal
5. Synthesis and confidence
6. Evidence ledger schema
7. Stopping rules

## 1. Question framing

Translate the user's request into an answerable question before searching. Use whichever parts apply:

- Population, system, or market.
- Intervention, exposure, technology, or policy.
- Comparator or baseline.
- Outcomes and decision thresholds.
- Time horizon, geography, and evidence cutoff.

Define ambiguous terms operationally. Record the interpretation when terms such as "effective," "safe," "best," "significant," or "large" affect inclusion.

Predeclare inclusion and exclusion rules. Common exclusions include inaccessible claims with no inspectable source, duplicate reports of the same study, irrelevant populations, superseded data releases, and sources outside the date or jurisdiction scope.

## 2. Search design

Build a query matrix rather than one giant query. Cover:

| Strand | Purpose | Typical terms |
|---|---|---|
| Core | Find direct evidence | exact topic + outcome |
| Synonyms | Avoid vocabulary bias | aliases, acronyms, earlier names |
| Evidence type | Find stronger designs | systematic review, meta-analysis, trial, benchmark, dataset |
| Mechanism | Test causal story | mechanism, pathway, failure analysis |
| Harms | Expose downsides | adverse, risk, limitation, incident |
| Contradiction | Challenge consensus | null, replication, critique, retraction, correction |
| Context | Test generalization | subgroup, region, year, environment |

Log query text, assignment ID, date, purpose, and the most useful result. Search snippets are leads, not evidence.

For scientific and technical questions, prioritize official repositories, publisher pages, DOI records, standards bodies, government datasets, and original product documentation. For current events or market claims, verify the event date separately from the publication date.

Use backward citation chasing for foundational evidence and forward chasing for replications, corrections, and later critiques. Check retraction or correction status for pivotal papers.

## 3. Source selection

Rank sources by fitness for the exact claim, not prestige alone. A primary dataset can be best for a current statistic; a systematic review can be best for an overall effect; an official standard can be best for a requirement.

Prefer this rough hierarchy when sources are equally direct:

1. Systematic reviews or meta-analyses with transparent methods.
2. Strong primary studies, official datasets, standards, and regulator reports.
3. Reputable guidelines or technical syntheses that expose their sources.
4. Preprints and working papers, labeled as not peer reviewed.
5. Expert commentary, news, vendor content, and blogs for context or discovery.

Do not count multiple articles based on the same study or dataset as independent confirmation. Use `independence_group` in the ledger to record shared data, authorship, funding, or source lineage.

## 4. Critical appraisal

Evaluate every source used for a key claim across these dimensions:

- **Design:** Is the design capable of supporting the claim, especially causality?
- **Directness:** Does it measure the requested population, intervention, comparator, and outcome?
- **Execution:** Are sampling, controls, measurement, missing data, and analysis credible?
- **Precision:** Are uncertainty intervals and sample sizes adequate for the conclusion?
- **Consistency:** Do independent sources agree in direction and approximate magnitude?
- **Bias:** Consider confounding, selection, measurement, selective reporting, and publication bias.
- **Transparency:** Are methods, data, code, funding, and conflicts disclosed?
- **Currency:** Is the evidence still applicable as of the cutoff date?

For quantitative evidence, capture sample size, effect estimate, uncertainty interval, units, and exact table/figure/page when available. Prefer absolute risks or real-world magnitudes alongside relative changes.

For benchmarks, check data contamination, test-set representativeness, baselines, compute budgets, evaluation variance, and whether comparisons used equivalent conditions.

For policy or market research, distinguish law or official data from forecasts, lobbying positions, marketing claims, and analyst opinion.

## 5. Synthesis and confidence

Synthesize by claim, not by source. For each conclusion:

1. State the narrowest claim the evidence supports.
2. Identify the strongest direct evidence.
3. Identify independent corroboration.
4. State contradictory evidence and plausible alternatives.
5. Explain applicability to the user's question.
6. Assign confidence and name the dominant reason.

Do not use vote counting. Ten weak reports do not outweigh one decisive high-quality study. Do not pool incomparable outcomes or populations merely to produce a number.

Treat confidence as evidence confidence, separate from decision importance. A low-confidence possibility may still matter if consequences are severe.

## 6. Evidence ledger schema

Store UTF-8 JSON with this structure:

```json
{
  "research_question": "Exact frozen question",
  "as_of_date": "YYYY-MM-DD",
  "protocol": {
    "scope": "Population/system, comparison, outcomes, geography, and dates",
    "inclusion_rules": ["Rule"],
    "exclusion_rules": ["Rule"],
    "confidence_rule": "How confidence will be assigned"
  },
  "search_log": [
    {
      "query": "Exact search query",
      "assignment_id": "A1",
      "date": "YYYY-MM-DD",
      "purpose": "Core, harms, contradiction, or other strand",
      "useful_result": "S1 or none"
    }
  ],
  "sources": [
    {
      "id": "S1",
      "title": "Source title",
      "url": "https://...",
      "publisher": "Journal, institution, or organization",
      "publication_date": "YYYY-MM-DD or YYYY",
      "accessed_date": "YYYY-MM-DD",
      "inspected": true,
      "inspected_by": "A1",
      "inspection_notes": "Opened full text; examined methods and Table 2",
      "source_type": "systematic_review",
      "primary": false,
      "peer_reviewed": true,
      "independence_group": "dataset-or-study-family-A",
      "relevance": "Why this source is included",
      "limitations": "Most important limitations"
    }
  ],
  "claims": [
    {
      "id": "C1",
      "text": "Narrow, testable claim",
      "importance": "key",
      "status": "supported",
      "confidence": "moderate",
      "confidence_reason": "Main reason for this rating",
      "evidence": [
        {
          "source_id": "S1",
          "locator": "Table 2, page 7",
          "support": "What the source directly shows"
        }
      ],
      "counterevidence": [],
      "notes": "Boundary conditions or calculation details"
    }
  ],
  "conflicts": [
    {
      "topic": "Point of disagreement",
      "source_ids": ["S1", "S2"],
      "resolution": "Resolved explanation or explicitly unresolved"
    }
  ],
  "protocol_deviations": [],
  "limitations": ["Limitation of the overall investigation"]
}
```

Allowed `source_type` values:

`systematic_review`, `meta_analysis`, `randomized_trial`, `observational_study`, `primary_research`, `official_data`, `regulation`, `standard`, `guideline`, `technical_documentation`, `preprint`, `expert_opinion`, `news`, `vendor_content`, `other`.

Allowed claim values:

- `importance`: `key` or `supporting`
- `status`: `supported`, `mixed`, `uncertain`, or `unsupported`
- `confidence`: `high`, `moderate`, `low`, or `very_low`

Use `peer_reviewed: null` when the concept does not apply. Make `locator` precise enough for another researcher to find the evidence. For web pages without stable pages or sections, use the visible heading and a short description; do not paste long copyrighted passages.

Use `publication_date: null` for an undated source rather than inventing a date.
`as_of_date` is the evidence cutoff: exclude sources published later. A year-only
date in the cutoff year cannot establish eligibility before a midyear cutoff;
verify the full date or disclose and resolve that uncertainty. Search and access
dates record retrieval and may be later than the evidence cutoff.
`accessed_date` remains required. Include only inspected sources in the ledger;
keep inaccessible candidates and retrieval failures in `limitations`. Identify
the investigator or lead in `inspected_by` and describe the material actually
read in `inspection_notes`. These fields record provenance, not proof of truth.
The lead confirms pivotal support and decisive counterevidence when integrating
findings; source/citation verification may also be delegated explicitly. A paper's abstract alone cannot establish details
from its inaccessible methods or tables.

Merge subagent fragments into this one ledger using the handoff rules in
[subagents.md](subagents.md). Save fragment provenance before remapping IDs.
Two agents finding the same study still count as one evidence group. Use the
same `independence_group` for sources based on that study or dataset.

## 7. Stopping rules

Assess whether the scoped questions have adequate support or defensible explicit
evidence gaps, whether relevant competing explanations/disagreements have been
examined, and whether another investigation could materially improve the answer.
Use further waves when valuable within budget. No fixed query/source count or
number of rounds establishes adequacy. A narrow official fact may need one
inspectable authoritative source; a contested causal inference needs more
careful appraisal and corroboration. Counts never determine confidence alone.

Stop when the answer is sufficiently supported, further applicable evidence has
little expected value, or a user/access/tool limit requires stopping. Record the
reason and remaining uncertainty. Under a user cap, reserve synthesis, citation
verification and artifact QA before consuming the research allowance. A cap
alone does not make a complete answer bounded; missing coverage or checks do.
