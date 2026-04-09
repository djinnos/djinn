---
title: Project memory broken-link and orphan backlog triage
type: design
tags: ["memory","triage","broken-links","orphans","planner"]
---

# Project memory backlog triage (2026-04-09)

Context: after the empty-folder default bug was fixed, `memory_health()` now agrees with detail tools in showing a real backlog rather than an aggregate/detail mismatch. Current evidence at triage time:

- `memory_health()`: 151 broken links, 561 orphans
- `memory_broken_links()`: populated project-wide detail list
- `memory_orphans()`: populated project-wide detail list

## Broken-link buckets

### 1. Legacy title-style / shorthand wikilinks in older canonical notes — **actionable note cleanup**
This is the dominant broken-link pattern.

Observed repeated raw_text examples:
- `Roadmap`
- `V1 Requirements`
- legacy ADR titles such as `ADR-009: Simplified Execution — No Phases, Direct Task Dispatch`
- shortened or variant ADR references such as `ADR-006`, `ADR-009`, `ADR-014`, `ADR-022`
- non-permalink labels like `Reply Loop Nudge and Marker System`
- formatting variants like `ADR-023 Cognitive Memory Architecture`

These appear across older ADR, design, reference, and research notes. The problem is not missing memory detail output anymore; the problem is that many historical notes still link by display title or singleton heading text instead of existing note permalinks.

Implication:
- This bucket should be treated as **real cleanup work in note content**.
- The likely fix is targeted replacement of title-style links with canonical permalinks, not tooling changes to `memory_broken_links()`.

Known supporting evidence already exists in case note [[cases/canonical-memory-maintenance-identified-exact-adr-link-replacements-and-stale-roadmap-status-text]].

### 2. A few likely genuinely missing or renamed targets — **small follow-up during cleanup pass**
Some raw texts do not look like canonical note permalinks or stable note titles and may represent renamed, removed, or never-created targets:
- `Reply Loop Nudge and Marker System`
- some old shorthand references in older reference documents
- some generic labels like `Roadmap` when no singleton/canonical note is intended

These should be triaged during the cleanup pass, but they are a minority relative to bucket 1. No separate project-wide tooling task is needed before attempting content cleanup.

### 3. Prior aggregate/detail contradiction — **already resolved, not part of remaining backlog**
The earlier "counts nonzero but detail empty" issue was a tool/default-parameter bug, already captured by [[cases/default-optional-folder-filters-must-map-to-null-not-empty-string-for-memory-detail-tools]].

Decision:
- Do **not** reopen the detail-tool bug as part of this backlog.
- Remaining broken links should be treated as note-content debt unless a cleanup pass finds a second concrete defect.

## Orphan buckets

### 1. `reference/repo-maps` (58 observed orphans) — **benign / intentionally tolerated inventory**
The orphan list contains a large block of hash-named repository maps under `reference/repo-maps/*`.

Decision:
- Treat repo maps as **intentional low-link reference artifacts**, not emergency cleanup.
- Future patrols should not escalate repo-map orphan volume by itself unless there is evidence the maps are supposed to be linked from stable index notes and are not.

### 2. `cases` (149 observed orphans) — **mostly benign historical/session-derived knowledge inventory**
The orphan list is dominated by many `cases/*` notes extracted from sessions. These are narrow historical learnings with localized scope paths and are often useful through search/retrieval rather than explicit backlinking.

Decision:
- Treat the bulk of `cases` orphan volume as **acceptable memory inventory**, not immediate manual-link debt.
- Only individual high-value case notes should be linked deliberately when they become canonical references for an active epic/task.

### 3. Top-level `reference`, `research`, and some `requirements` notes — **mixed; review only when they are canonical current docs**
Observed orphan counts include:
- top-level `reference`: 19 observed
- `research`: 5 observed in detail output subset highlighted during triage, though `memory_list` shows a broader research set where many older survey notes are likely intentionally sparse
- `requirements`: 4 roadmap-style notes plus `requirements/v1-requirements`

Decision:
- These are **not automatically benign**, but they are not a project-wide emergency either.
- Prioritize cleanup only for canonical singleton/current-note surfaces like `roadmap`, `brief`, and `requirements/v1-requirements`, where broken links or stale status text affect patrol interpretation.
- Old surveys/WIP research notes can remain orphaned if they serve archival retrieval rather than navigational documentation.

## Canonical triage outcome

### What requires follow-up
1. **Actionable cleanup task** for broken-link note-content debt, focused on legacy title-style ADR/singleton wikilinks and any stale canonical note text discovered while fixing them.
2. During that task, optionally decide whether memory-health/reporting should explicitly bucket or suppress intentional orphan-heavy folders (`cases`, `reference/repo-maps`) so patrols do not treat them as undifferentiated emergencies.

### What does NOT require broad cleanup
- Mass-linking `reference/repo-maps/*`
- Mass-linking all `cases/*`
- Reopening the fixed aggregate/detail mismatch bug

## Patrol guidance going forward

When patrol sees high memory-health counts:
- Broken links should be treated as actionable primarily when they match the legacy title-style / shorthand-link bucket above.
- Orphan volume in `cases` and `reference/repo-maps` should be treated as **background inventory unless coupled with a canonical-note defect**.
- Canonical singletons/current docs (`roadmap`, `brief`, active requirement notes) remain worth checking because they influence board-health interpretation.

## Routed next action

Created follow-up planning task [[Route actionable project-memory cleanup after backlog triage]] to own the narrow actionable cleanup without reopening project-wide triage.
