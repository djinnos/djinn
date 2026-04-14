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

Known supporting evidence already exists in the case note `cases/canonical-memory-maintenance-identified-exact-adr-link-replacements-and-stale-roadmap-status-text`.

### 2. A few likely genuinely missing or renamed targets — **small follow-up during cleanup pass**
Some raw texts do not look like canonical note permalinks or stable note titles and may represent renamed, removed, or never-created targets:
- `Reply Loop Nudge and Marker System`
- some old shorthand references in older reference documents
- some generic labels like `Roadmap` when no singleton/canonical note is intended

These should be triaged during the cleanup pass, but they are a minority relative to bucket 1. No separate project-wide tooling task is needed before attempting content cleanup.

### 3. Prior aggregate/detail contradiction — **already resolved, not part of remaining backlog**
The earlier "counts nonzero but detail empty" issue was a tool/default-parameter bug, already captured by the case note `cases/default-optional-folder-filters-must-map-to-null-not-empty-string-for-memory-detail-tools`.

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

Created follow-up planning task "Route actionable project-memory cleanup after backlog triage" to own the narrow actionable cleanup without reopening project-wide triage.


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




## Prioritized orphan backlog refresh (2026-04-13)

Current measurement from `memory_health()` / `memory_orphans()`:
- `memory_health().orphan_note_count`: **797** gross orphans
- `reference/repo-maps/*`: **66** orphan notes — intentional/generated repo-map artifacts
- `cases/*`: **225** orphan notes — mostly tolerated historical/session inventory, but not generated
- `requirements/*`: **4** orphan notes — all roadmap-style requirement notes for already-complete epics
- `research/*`: **6** orphan notes — mixed archival surveys and WIP implementation scratchpads
- top-level `reference/*`: **20** orphan notes — mixed inventory/test-fixture/reference debt

### Updated split for patrol interpretation

Use three interpretation buckets instead of treating the 797 total as one cleanup backlog:

1. **Intentional/generated noise — 66 notes**
   - `reference/repo-maps/*`
   - These are the expected generated artifact slice. Future patrols should treat this folder as baseline noise unless a stable index note is later supposed to backlink them.

2. **Tolerated retrieval-oriented inventory — 225+ notes**
   - dominated by `cases/*`
   - Most case notes remain acceptable as search-first historical memory and should not trigger broad manual relinking.
   - This bucket is *not* the same as generated noise, but it also should not be treated as emergency cleanup by count alone.

3. **Actionable knowledge-note candidates — currently smallest, highest-value slice**
   - `requirements/*` orphan roadmaps: 4 notes
   - `research/*` orphan notes: 6 notes
   - selected active-epic `cases/*` clusters whose guidance should be consolidated into canonical notes instead of staying as unlinked duplicates

### High-value first cleanup slice

The first cleanup batch should target the **active semantic-memory orphan case cluster**, not the completed-epic roadmap requirements or older archival research notes.

Recommended first batch:
- `cases/embedding-runtime-seam-added-for-semantic-memory`
- `cases/thread-semantic-search-context-through-bridge-and-state-layers`
- `cases/blend-semantic-retrieval-into-existing-note-search-without-changing-the-mcp-interface`
- `cases/added-vector-aware-ranking-to-the-existing-note-search-pipeline`
- `cases/blend-semantic-vector-search-into-existing-memory-search-ranking`

Rationale:
- all five are recent orphan case notes tied to the still-active semantic-memory epic (`h1yj`)
- they duplicate or refine guidance that belongs in canonical ADR/roadmap/design surfaces for active work
- several are clearly sequential session extracts for the same implementation story, so they are strong candidates for **linking from the roadmap/ADR context, merging into canonical notes, or deprecating as redundant session residue**
- cleaning this slice will improve future retrieval around active work more than touching completed-epic roadmap notes or old survey research

### Lower-priority actionable slice after the semantic-memory batch

1. `requirements/*` orphan roadmap notes for completed epics:
   - [[requirements/delete-stale-canonical-graph-shims-from-mcp-bridge-rs-roadmap]]
   - [[requirements/remove-verified-dead-code-across-agent-repo-map-repo-graph-roadmap]]
   - [[requirements/split-oversized-production-hubs-agent-mcp-bridge-roadmap]]
   - [[requirements/split-oversized-test-files-by-scenario-module-roadmap]]

   These are likely deprecation/archive candidates because each note already records epic completion and no longer appears to serve active navigation.

2. `research/*` orphan notes:
   - archival surveys like [[research/embedded-database-survey-2026]] and [[research/rust-agentic-ecosystem-2026]] are acceptable to leave orphaned unless a canonical note should cite them
   - WIP implementation scratchpads like [[research/djinn-mcp-extraction-wip]] and [[research/djinn-mcp-wiring-wip]] are better deprecation/consolidation candidates than link targets

### Patrol rule update

For future orphan patrols:
- treat **66 repo-map orphans** as the intentional/generated baseline noise floor
- do **not** open broad cleanup work because the `cases/*` bucket is large
- prefer narrow follow-up tasks only when orphan findings cluster around active epics or clearly redundant canonical-adjacent notes
- use the semantic-memory case cluster above as the current named first cleanup batch

## Residual historical ADR-title / shorthand alias classification (2026-04-13)

Latest sampling of `memory_broken_links()` confirms that the remaining broken-link backlog is now dominated by a **legacy historical alias slice** concentrated in `decisions/*` and `reference/*`, not by fresh canonical-note regressions.

### Sampled residual patterns

#### A. Generic singleton shorthand: `Roadmap`
Representative sources:
- `decisions/adr-001-desktop-only-clerk-authentication-via-system-browser-pkce`
- `decisions/adr-004-execution-settings-per-role-model-priority-and-per-model-session-limits`
- `decisions/adr-045-sse-event-batching-and-knowledge-base-housekeeping`
- `reference/agent-harness-scope`
- `reference/session-compaction-scope`
- `reference/structured-session-finalization-scope`

Classification: **mixed historical debt, mostly tolerable rather than urgent content defect**.

Rationale:
- In older ADR/reference notes, the legacy shorthand `Roadmap` was often used for “the roadmap note/current plan” rather than a stable permalink contract.
- There is now a canonical singleton `roadmap`, but mass-normalizing every historical `Roadmap` occurrence across legacy notes would be broad editorial churn with low operational value.
- Recent/current canonical notes were already cleaned up in prior tasks; what remains is mostly archival language in historical notes.

Recommended action:
- **Do not treat this slice as parser/index noise**; these are real unresolved links in note content.
- **Do not reopen a project-wide mass cleanup** just for `Roadmap`.
- Allow `Roadmap` to remain as **tolerated historical debt** except when it appears in a currently maintained canonical note whose navigation quality matters for patrols.

#### B. Full ADR-title aliases and title-cased variants
Representative sources:
- `ADR-009: Simplified Execution — No Phases, Direct Task Dispatch`
- `ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning`
- `ADR-042: DB-Only Knowledge Extraction, Consolidation, and Task-Routing Fixes`
- `ADR-043 Repository Map — SCIP-Powered Structural Context for Agent Sessions`
- `ADR-053 Semantic Memory Search — Candle Embeddings with sqlite-vec`
- title-case variants such as `ADR-023 Cognitive Memory Architecture`, `ADR-029 Vertical Workspace Split`, `ADR-034 Agent Role Hierarchy`
- shorthand forms such as `ADR-006`, `ADR-009`, `ADR-014`, `ADR-022`

Classification: **real content debt, but split into two sub-buckets**.

1. **Historical archival references in older ADR/reference notes** → **tolerated historical debt unless touched for another reason**.
   - These links were authored before canonical permalink conventions stabilized.
   - They are numerous and repetitive, especially inside old ADR relation sections.
   - Fixing them all is content normalization work, not tooling repair.

2. **Recent/high-value canonical notes that are still actively used** → **narrow actionable cleanup**.
   Representative current notes from the sample:
   - `decisions/adr-045-sse-event-batching-and-knowledge-base-housekeeping`
   - `decisions/adr-053-semantic-memory-search-candle-embeddings-with-sqlite-vec`
   - `reference/repository-understanding-and-memory-freshness-upgrade-path`

   These notes are recent, cross-link-rich, and likely to be reused during active planning/patrol work. In these cases the broken ADR-title links are better treated as **content normalization targets** rather than tolerated archival debt.

Recommended action:
- Treat the broad historical tail as **content debt to tolerate**, not as parser/index noise.
- Prefer **targeted content normalization** in a very small current-note subset instead of alias/index support or broad historical editing.
- Do **not** add generalized alias support for title-case ADR labels right now; doing so would encode unstable historical titles/typography into memory resolution rules.

#### C. Non-note prose accidentally parsed as wikilinks or unresolved placeholders
Representative examples from sampled output:
- `wikilinks` in `decisions/adr-023-cognitive-memory-architecture-multi-signal-retrieval-and-associative-learning`
- `target` in `decisions/adr-045-sse-event-batching-and-knowledge-base-housekeeping`
- `Autoresearch Reference`, `Paperclip Reference`, `Ruflo Comparison Analysis`, `Djinn Namespace Git Sync`

Classification: **parser/index noise or genuine unresolved placeholder text, depending on source context**.

Rationale:
- Some of these are clearly not intended canonical note permalinks and likely originate from prose examples or placeholders wrapped in wikilink syntax.
- They are a minority compared with the ADR-title / `Roadmap` backlog and should not drive broad policy.

Recommended action:
- Handle only when encountered in a narrow current-note cleanup pass.
- Do not open a dedicated tooling task unless these placeholder-style targets become a substantial share of the backlog.

### Decision summary for patrols

The residual `decisions/*` and `reference/*` broken-link slice should be interpreted in three buckets:

1. **Narrow actionable cleanup**
   - recent/high-value canonical notes still in active use
   - recommended fix: replace broken title-style links with canonical permalinks

2. **Tolerated historical alias debt**
- legacy ADR/reference notes whose broken links are mostly old title-case aliases or generic `Roadmap`
   - recommended fix: leave in place unless a note is otherwise being updated

3. **Minor parser/placeholder noise**
   - accidental non-note targets such as `wikilinks` or `target`
   - recommended fix: opportunistic cleanup only; no alias support and no separate broad tooling work

### Recommended next action

If follow-up cleanup is scheduled, keep it **narrow and current-facing**:
- normalize broken ADR-title/title-case links only in a tiny subset of high-value active notes (for example `decisions/adr-045-sse-event-batching-and-knowledge-base-housekeeping`, `decisions/adr-053-semantic-memory-search-candle-embeddings-with-sqlite-vec`, and `reference/repository-understanding-and-memory-freshness-upgrade-path`)
- do **not** reopen project-wide historical ADR/research/reference cleanup
- do **not** add alias-resolution support for generic `Roadmap` or historical ADR titles at the memory-tool layer

This classification makes future patrols cheaper: elevated counts from old `decisions/*` / `reference/*` notes should not be interpreted as fresh regressions unless the broken links are concentrated in currently maintained canonical notes.

Scope boundary for the current cleanup: normalize only the broken links in the currently maintained canonical notes named above; leave broad historical `Roadmap`, ADR-title alias, `cases/*`, and `reference/repo-maps/*` debt untouched outside this narrow pass.

