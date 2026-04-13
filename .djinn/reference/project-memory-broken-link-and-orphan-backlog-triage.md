---
title: Project memory broken-link and orphan backlog triage
type: 
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


## Follow-up decision: orphan-heavy folders reporting policy (2026-04-09)

Decision: **do not change `memory_health()` aggregate counting or suppress `memory_orphans()` detail output for `cases` or `reference/repo-maps` right now.** The narrow durable fix is to make patrol interpretation explicit in canonical triage/design context:

- `memory_health().orphan_note_count` remains a **gross inventory count** of all non-singleton, non-catalog orphan notes.
- `memory_orphans()` remains the **raw detail list** and should continue to show `cases/*` and `reference/repo-maps/*` entries.
- Planner/board-health patrols must treat orphan-heavy folders in two buckets:
  - **Tolerated inventory bucket:** `cases/*`, `reference/repo-maps/*`
  - **Actionable orphan debt bucket:** canonical current docs and other orphaned notes whose value depends on navigational linkage (for example `roadmap`-adjacent requirements, current reference notes, active design notes)

Rationale:

- The current tooling is behaving consistently after the empty-folder fix: the aggregate count is real, and the detail tool exposes the underlying notes.
- Suppressing intentional folders inside the tool output would hide legitimate detail and create a second interpretation of "orphan count".
- Adding new per-folder buckets to the MCP/API surface is possible later, but it is broader than needed to stop patrol churn today.
- Repo maps and case-history notes are intentionally retrieval-oriented inventory; mass-linking or excluding them from storage/index semantics would be the wrong cleanup target.

Operational rule for future patrols:

1. Use `memory_health()` to notice that orphan volume is elevated.
2. Use `memory_orphans()` / folder inspection to determine whether the volume is dominated by tolerated inventory (`cases`, `reference/repo-maps`).
3. Escalate only when orphan findings are concentrated in canonical/current-note surfaces or reveal a concrete documentation defect.
4. Do **not** open broad cleanup work solely because `cases` or `reference/repo-maps` dominate orphan counts.

This closes the earlier optional question in this note: the chosen policy is **documented patrol interpretation, not tooling suppression and not mass relinking**. A future tooling enhancement is only warranted if patrol operators still misread the gross orphan count after following this guidance.




## Follow-up triage refresh (2026-04-13)

Re-ran `memory_health`, `memory_broken_links`, and `memory_orphans` after the semantic-memory roadmap repair. Current counts are **138 broken links** and **726 orphans**.

### Broken-link classification refresh

#### 1. Singleton alias links (`[[roadmap]]`, `[[brief]]`) — **real legacy-content debt, defer broad fix / repair selectively**
These remain one of the most common raw-text targets in the broken-link report. Canonical singleton notes do exist as `[[roadmap]]` and `[[brief]]`, so this bucket is not a missing-note defect. It is legacy note content still linking by display title rather than canonical permalink.

Decision:
- Treat as **real cleanup debt in note bodies**, not a tooling bug.
- Do **not** mass-edit every historical note in patrol work.
- Focus any repair pass on canonical/current notes first; older archival notes can remain deferred.

#### 2. Legacy ADR title/shorthand variants — **real cleanup debt, highest-value focused follow-up**
The dominant remaining population is still ADR title drift: full old titles, shortened titles, shorthand `ADR-006`/`ADR-019`, punctuation variants, and pre-permalink display labels. Examples seen again in this refresh include:
- `ADR-009: Simplified Execution — No Phases, Direct Task Dispatch`
- `ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning`
- `ADR-023 Cognitive Memory Architecture`
- `ADR-034 Agent Role Hierarchy`
- `ADR-043 Repository Map — SCIP-Powered Structural Context`

Canonical ADR notes exist for many of these targets; the breakage comes from old wikilink text that no longer matches canonical permalinks/titles exactly.

Decision:
- This is the **highest-value actionable defect** remaining.
- Route one focused cleanup task for canonical/current notes that still use legacy ADR/singleton aliases, instead of opening another broad backlog-triage pass.

#### 3. Small genuinely missing/renamed-target subset — **repair opportunistically during focused cleanup**
A much smaller subset still looks like true missing or renamed targets rather than simple alias drift. Examples from the current report include:
- `Autoresearch Reference`
- `wikilinks`
- `Djinn Namespace Git Sync`
- `Cognitive Memory Scope`
- a few design-title variants that may need relinking to canonical permalinks

Spot checks show some of these already have nearby canonical notes under different permalinks/titles (for example `decisions/djinn-namespace-git-sync`, `reference/cognitive-memory-scope`). Others may simply be prose that was mistakenly written as a wikilink.

Decision:
- Treat this as a **small subset to verify during the focused cleanup pass**.
- Do not open a separate tool/reporting defect unless the repair pass finds a target that truly has no valid canonical replacement.

#### 4. False-positive/tooling bucket — **no new evidence of a fresh reporting bug**
The earlier aggregate/detail mismatch has not reappeared. Detail output is populated and consistent with the gross counts.

Decision:
- Broken-link noise is still best understood as content debt plus a small renamed-target subset.
- Do **not** reopen the earlier memory-health detail bug.

### Orphan refresh

#### `pitfalls/*` orphan concentration — **expected curation debt / retrieval-oriented inventory**
The orphan count remains heavily dominated by `pitfalls/*` inventory, similar to the earlier tolerated-orphan findings for `cases/*` and `reference/repo-maps/*`. The current detail output shows a wide spread of narrowly scoped pitfall notes rather than one obvious canonical note missing backlinks.

Decision:
- Treat the large `pitfalls` orphan block as **expected curation debt / retrieval-oriented inventory**, not an acute memory-tool defect.
- No evidence in this patrol pass suggests `memory_orphans()` is misreporting the folder.
- Only escalate future orphan work if a smaller subset of active canonical notes emerges that clearly should be linked now.

### Canonical outcome of this refresh

- **Actionable now:** one focused cleanup task for canonical/current notes with legacy ADR and singleton alias wikilinks; verify the small renamed-target subset while there.
- **2026-04-13 singleton-alias cleanup outcome:** historical `decisions/*`, plus matching `reference/*` and `research/*` notes in the scoped slice, were normalized from backticked legacy aliases (`Roadmap`, `Project Brief`, `V1 Requirements`) to canonical permalinks `[[roadmap]]`, `[[brief]]`, and `[[requirements/v1-requirements]]` where those canonical notes already exist.
- **Remaining broken-link follow-up after this pass:** treat only concrete renamed/missing-target exceptions such as `Autoresearch Reference`, `wikilinks`, `Djinn Namespace Git Sync`, and `Cognitive Memory Scope`; do not reopen broad singleton-alias triage.
- **Deferred as backlog:** broad historical-note replacement for all alias-style links.
- **Deferred as curation debt:** the large `pitfalls/*` orphan inventory.
- **Not a current tooling bug:** aggregate/detail reporting for memory health.
