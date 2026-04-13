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



## Orphan classification refresh (2026-04-13)

Fresh evidence from the current pass:

- `memory_health()`: **765 orphans / 844 total notes**
- `memory_orphans(folder="reference/repo-maps")`: **62** orphaned repo-map snapshots
- `memory_orphans(folder="requirements")`: **4** orphaned requirement notes
- `memory_orphans(folder="research")`: **6** orphaned research notes
- `memory_orphans(folder="cases")`: still a very large historical/session-derived slice; detail output remains dominated by `cases/*`, but this pass intentionally sampled/classified rather than trying to relink that whole bucket

### Refined buckets

#### 1. Intentional / generated orphan-heavy inventory — **ignore for patrol escalation by default**

**Confirmed bucket:** `reference/repo-maps/*` (**62 current orphans**)

Representative examples:
- `reference/repo-maps/00d81c76211b`
- `reference/repo-maps/4d941c575f21`
- `reference/repo-maps/7cf9c6ef3254`
- `reference/repo-maps/f62baaa142c8`

Why this bucket is tolerated:
- titles are hash-addressed `Repository Map <id>` snapshots rather than navigational docs
- the notes are generated/cache-like reference artifacts
- the backlog would remain dominated by these snapshots even if a few knowledge-note orphans were cleaned up

**Patrol rule:** treat repo-map orphan volume as background inventory unless a canonical index/navigator note starts claiming these snapshots should be explicitly linked.

#### 2. Historical retrieval-oriented knowledge inventory — **usually tolerated, sample only when current work needs it**

**Dominant non-generated bucket:** `cases/*`

Representative examples from the current orphan list:
- `cases/broken-link-backlog-shifted-from-roadmap-artifact-to-legacy-shorthand-adr-title-aliases`
- `cases/canonical-current-note-wikilinks-should-be-normalized-narrowly-without-expanding-backlog-cleanup`
- `cases/embedding-runtime-seam-added-for-semantic-memory`
- `cases/stale-memory-index-can-contradict-repaired-canonical-singleton-notes`

Classification:
- these are mostly session-derived learnings retrievable by search/context-building rather than by manual backlink navigation
- many are useful as point references for an active task or epic, but mass-linking the whole folder would be low-value cleanup

**Patrol rule:** do not treat `cases/*` orphan count as undifferentiated debt. Only escalate when a specific case becomes canonical guidance for active work and still lacks the one or two backlinks that would make it discoverable from that canonical note.

#### 3. Actionable orphan slice — **small, scoped knowledge cleanup candidates**

This pass found a narrow set worth routing into future cleanup work because the notes read like canonical planning/reference artifacts rather than disposable history.

**Requirement-roadmap notes (4 current orphans):**
- `requirements/delete-stale-canonical-graph-shims-from-mcp-bridge-rs-roadmap`
- `requirements/remove-verified-dead-code-across-agent-repo-map-repo-graph-roadmap`
- `requirements/split-oversized-production-hubs-agent-mcp-bridge-roadmap`
- `requirements/split-oversized-test-files-by-scenario-module-roadmap`

Why actionable:
- these are roadmap-shaped planning notes in `requirements/*`, not generated artifacts
- they likely want either an inbound link from a canonical triage/roadmap note, or a deprecation/archive decision if the wave is complete and the note is no longer meant to be navigated

**Research notes (6 current orphans):**
- `research/rust-compilation-and-tooling-optimization-strategy`
- `research/embedded-database-survey-2026`
- `research/rust-agentic-ecosystem-2026`
- `research/goose-library-integration-research-phase-5`
- `research/djinn-mcp-extraction-wip`
- `research/djinn-mcp-wiring-wip`

Why mixed/actionable:
- the first four look like durable reference research that may deserve linkage from canonical requirements/design/ADR notes when still active
- the two `djinn-mcp-*wip` notes look more archival and may instead deserve explicit deprecation/archive handling rather than new backlinks

### Narrow next actions

Do **not** open broad orphan-backlog cleanup. If future patrols want concrete follow-up, keep it to one of these small slices:

1. **Requirement-roadmap orphan pass (preferred first follow-up):** decide for the 4 orphaned `requirements/*roadmap` notes whether each should gain one canonical inbound link or be explicitly retired/deprecated.
2. **Research-note curation pass (optional, max 4–6 notes):** classify durable research references vs archival WIP notes, then either add one canonical backlink or mark them archival.
3. **Case-note linking only by demand:** when an active epic repeatedly cites a specific orphaned case, add the targeted backlink then; do not mass-link `cases/*`.

### Updated patrol guidance

When `memory_health().orphan_note_count` looks alarming:

- first subtract the confirmed tolerated repo-map slice (**62 currently**) mentally
- then assume `cases/*` is mostly retrieval-oriented historical inventory unless a specific active canonical doc depends on one of those notes
- route cleanup attention to small canonical-ish slices (`requirements/*roadmap`, durable `research/*`, current design/reference docs) instead of the raw gross orphan total

This refresh keeps the earlier policy unchanged: **documented interpretation beats tooling suppression**, and the actionable orphan debt should be handled as small targeted follow-ups rather than a project-wide relinking campaign.
