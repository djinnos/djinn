---
title: Narrow current-note memory hygiene follow-up
type: reference
tags: ["memory","hygiene","broken-links","orphans","planner"]
---

# Narrow current-note memory hygiene follow-up (2026-04-14)

## Context
Follow-up to [[reference/project-memory-broken-link-and-orphan-backlog-triage]] after refreshed counts from `memory_health()` showed **124 broken links** and **1027 orphans**. The backlog is still dominated by tolerated historical alias debt and orphan-heavy inventory, so this plan intentionally names only a tiny current-note cleanup slice.

## Current evidence snapshot
- `memory_health()`: 124 broken links, 1027 orphans
- `memory_broken_links()`: remaining backlog is still mostly legacy title-style ADR aliases and generic `[[Roadmap]]` links in older `decisions/*` and `reference/*` notes
- `memory_orphans()`: orphan volume is heavily concentrated in `cases/*` plus `reference/repo-maps/*`

## Decision
Do **not** open broad historical alias cleanup.

The actionable follow-up is a **narrow canonical-note normalization pass** for a very small set of current/high-value notes whose broken title-style links still affect active planning and patrol interpretation.

## Named broken-link cleanup slice
Normalize broken wikilinks only in these current canonical notes:

1. [[decisions/adr-057-proposal-fuse-mounted-memory-filesystem-as-the-primary-agent-interface]]
   - normalize title-style ADR proposal links to canonical permalinks for ADR-054, ADR-055, ADR-056, and ADR-023
   - remove or plain-text any example-only placeholder wikilinks such as `[[Note Title]]` / `[[wikilinks]]`
2. [[decisions/adr-053-semantic-memory-search-candle-embeddings-with-sqlite-vec]]
   - normalize the remaining title-style `Related:` links to canonical permalinks for ADR-023 and ADR-042
3. [[reference/repository-understanding-and-memory-freshness-upgrade-path]]
   - keep the note current-facing by normalizing its active ADR references to canonical permalinks where needed during the same pass
4. [[reference/project-memory-broken-link-and-orphan-backlog-triage]]
   - replace legacy `[[Roadmap]]` prose mention with canonical `[[roadmap]]` or plain text as appropriate
   - replace the task-title wikilink `[[Route actionable project-memory cleanup after backlog triage]]` with plain text/task id commentary because task titles are not canonical note targets

These four notes are small enough for one maintenance task and are high-value because they are still reused by planner/patrol work.

## Explicitly excluded from this slice
### Tolerated historical broken-link debt
Leave these outside the cleanup scope unless a note is otherwise being edited:
- older ADR/reference notes whose unresolved links are mostly full ADR-title aliases or shorthand like `ADR-006`, `ADR-009`, `ADR-014`, `ADR-022`
- generic historical `[[Roadmap]]` shorthand in archival notes
- minor parser/placeholder noise in older historical notes unless encountered while fixing one of the current notes above

Rationale: these are real broken links, but they are mostly historical editorial debt rather than active canonical-note defects. Broad normalization would create large churn for low operational value.

### Orphan-heavy tolerated inventory
Treat these as inventory, not the target of this follow-up:
- `cases/*` — retrieval-oriented historical/session-derived knowledge; only individual active-epic clusters should be linked or consolidated deliberately
- `reference/repo-maps/*` — intentional/generated reference artifacts; orphan status alone is not a defect

## Actionable orphan classification
### Not actionable by folder count alone
- `cases/*`
- `reference/repo-maps/*`

### Actionable only when tied to active canonical defects
- current `reference/*`, `design/*`, `requirements/*`, and recent ADR notes that are part of active planning/patrol surfaces

For the present follow-up, the next actionable slice is the broken-link normalization above; orphan count should remain interpreted through the backlog-triage bucket guidance instead of triggering mass relinking.

## Scope note for future patrols
Legacy `Roadmap` aliases and older ADR-title/title-case aliases remain **tolerated historical debt** outside this narrow cleanup scope. Patrols should escalate only when broken links concentrate in current canonical notes or when orphan findings expose a concrete current-note navigation defect.

## Recommended execution shape
One narrow maintenance task should update only the four named notes above, verify `memory_broken_links()` no longer reports their specific broken targets, and leave the broader historical backlog untouched.

## Relations
- [[reference/project-memory-broken-link-and-orphan-backlog-triage]]
- [[decisions/adr-057-proposal-fuse-mounted-memory-filesystem-as-the-primary-agent-interface]]
- [[decisions/adr-053-semantic-memory-search-candle-embeddings-with-sqlite-vec]]
- [[reference/repository-understanding-and-memory-freshness-upgrade-path]]
