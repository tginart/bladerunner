# Research report and optional PDF specification

## Contents

1. Deliverables
2. Report structure
3. Writing and citation rules
4. Tables and figures
5. Visual system
6. Verification checklist

## 1. Deliverables

Choose the structure, length and format for the user's question. Deliver a
Markdown report (`report.md`) and machine-readable `evidence.json`. Add a polished
PDF (`report.pdf`) when requested or useful and generation/inspection tools are
available. An optional PDF is not required for full research completion; an
explicitly requested PDF remains an outstanding deliverable until provided.
Record the chosen formats in `run.json`. If PDF generation or page inspection
fails, deliver available work and identify the missing artifact/check. Do not
claim visual verification without inspecting the final rendered pages.

Keep the report generator, rendered page images, downloaded papers, and scratch notes as working files unless the user requests them.

## 2. Report structure

Use this order unless the subject needs a justified variation:

1. Cover: title, precise subtitle/question, evidence cutoff, report date.
2. Executive summary: direct answer, 3-7 key findings with confidence, decision implications, and most important caveat.
3. Scope and definitions: what was and was not investigated.
4. Methodology: protocol, search strategy, inclusion/exclusion, appraisal method, and deviations.
5. Findings: organized by question or causal chain, not by source.
6. Contradictions and alternative explanations.
7. Decision implications or recommendations, only when requested or justified.
8. Limitations and evidence gaps.
9. Conclusion: narrow answer that matches the evidence.
10. References: complete, human-readable citations with live URL or DOI.
11. Optional appendix: query log, detailed evidence tables, calculations, or glossary.

Place a short confidence explanation beside each executive-summary finding. Ensure a reader can distinguish reported source facts, the report's calculations, and the report author's inference.

Describe the agent assignments and review stages actually completed. Include
failed or skipped strands that limit the conclusion. Multiple agents agreeing
does not raise evidence confidence without independent underlying sources.

## 3. Writing and citation rules

Lead with the answer. Use concrete language, short paragraphs, descriptive headings, and informative captions. Define technical terms at first use.

Attach citations to the sentence or paragraph they support. Cite every externally verifiable number, quotation, current fact, and non-obvious factual assertion. Prefer stable primary URLs, DOI links, or official records. Do not expose internal search reference IDs or raw tool tokens.

Use a consistent reference style appropriate to the output, such as linked
citations in Markdown or numeric references `[1]` in a PDF. Give every reference author or institution, title, publisher/journal, year/date, and URL/DOI. Include access dates for changeable web pages.

Paraphrase sources. Quote only when wording itself is evidence and keep quotations short. Never use a secondary source as if it were the primary source.

Keep confidence language consistent with the ledger. Present ranges and uncertainty intervals when they matter. State assumptions next to calculations and make calculations reproducible in the appendix.

## 4. Tables and figures

Use a visual only when it compresses real information better than prose. Good defaults include:

- Evidence table: claim, best evidence, counterevidence, confidence, reason.
- Comparison table: alternatives across the user's decision criteria.
- Timeline: evidence or policy changing over time.
- Forest-style plot or interval chart: comparable quantitative estimates.
- Flow diagram: mechanism or causal chain with uncertainty marked.

For each visual:

- Number and title it.
- Label axes, units, populations, and dates.
- Cite the underlying data beneath it.
- State whether values are copied, transformed, or calculated.
- Use color plus labels or patterns; never rely on color alone.
- Avoid pseudo-precision and decorative charts.

## 5. Visual system

Create a calm technical report, optimized for reading and printing:

- US Letter or A4 pages with 0.65-0.85 inch margins.
- One readable sans-serif family for body and headings, with a reliable Unicode fallback.
- Body text around 9.5-11 pt and line spacing around 1.25-1.4.
- Strong title, clear section hierarchy, restrained accent color, and generous whitespace.
- Running header with shortened title; footer with page number and evidence cutoff.
- Tables with repeated header rows, sensible column widths, and no clipped cells.
- Links visibly distinct in color and clickable in the PDF.
- High-resolution raster images or vector charts.
- ASCII hyphens instead of fragile Unicode dash characters.

Use document/PDF tools available in the current environment. A local Python
generator such as ReportLab and a renderer such as Poppler are suitable; check
availability before relying on them. Do not assume Codex-only tools, a separate
PDF skill, or an installed font/Python package exists in Bladerunner. Keep any
generator paths and dependency choices in working notes so the report can be
reproduced. Use a font with the required glyphs rather than silently dropping
characters.

Avoid dense wall-to-wall text, excessive boxes, tiny references, orphan headings, single-line spill pages, and awkward table splits.

## 6. Verification checklist

### Evidence integrity

- The title and conclusion answer the same frozen question.
- Every key finding maps to a ledger claim.
- Every cited source was opened and inspected.
- Citations point to the intended source and are not duplicated under misleading names.
- Numbers, denominators, dates, units, and uncertainty match their source.
- Independent confirmation really uses distinct `independence_group` values.
- Contradictory evidence and unresolved uncertainty are visible in the main report.
- Executive-summary confidence is no stronger than body evidence.

### PDF integrity

- Open the final PDF with a parser and confirm page count is plausible.
- Extract text and spot-check the title, key findings, conclusion, and references.
- Render every page to PNG after the final edit.
- Inspect every rendered page at readable resolution.
- Confirm no clipped text, overlap, missing glyphs, broken tables, blank pages, or low-resolution visuals.
- Confirm page numbers, headers, footers, references, and section transitions are consistent.
- Confirm URLs are human-readable and clickable where supported.
- Confirm the delivered file is the final inspected version.
