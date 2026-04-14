---
title: ADR-054 Roadmap — Memory Extraction Quality Gates and Note Taxonomy
type: design
tags: ["adr-054","roadmap","memory","broken-links","orphans","classification"]
---

# ADR-054 Roadmap — Memory Extraction Quality Gates and Note Taxonomy

## Status
In progress — implementation work is largely landed, with final closure verification still running.

## Goal
Tighten extraction quality in `llm_extraction.rs` so durable memory writes are gated by stronger note taxonomy, structured templates, semantic novelty checks, and real access signals instead of permissive session-extraction defaults.

## Landed work
- Extraction quality-gate decisions implemented in `llm_extraction.rs`.
- Structured templates enforced for durable `pattern` / `pitfall` / `case` notes.
- Working Spec routing added for non-durable extracted knowledge.
- MCP memory search/retrieval access tracking extended so freshness signals are real.
- Corpus audit tooling landed for ADR-054 cleanup classification.
- Narrow roadmap/design canonical-link cleanup landed for current planning artifacts.

## Remaining closure tasks
- `8vh1` — verify the corpus cleanup pass and record before/after evidence.
- `lnvm` — reconcile final canonical memory refs and closure artifacts.

## Residual project-memory backlog classification (2026-04-14)

Current project-wide memory health sampled during this pass:
- `memory_health()`: **101 broken wikilinks**, **930 orphan notes**, **1042 total notes**.

This classification pass is intentionally narrow: it separates tolerated backlog from actionable current-note debt so later cleanup can stay surgical.

### Broken-link buckets

#### 1. Confident canonical-target defects in current notes — narrow actionable cleanup
This is the smallest but highest-value slice.

Confirmed examples:
- `[[design/working-spec-adr-055-sqlite-seam-inventory]]` is still referenced from [[cases/adr-roadmap-captured-sqlite-migration-seam-inventory-categories]] and remains unresolved.
- Recent/high-value canonical notes previously identified in backlog triage remain the right place for any further narrow normalization if they still carry broken ADR-title/title-case links:
  - [[decisions/adr-045-sse-event-batching-and-knowledge-base-housekeeping]]
  - [[reference/repository-understanding-and-memory-freshness-upgrade-path]]
  - [[reference/project-memory-broken-link-and-orphan-backlog-triage]]

Classification:
- Treat these as **current-note defects** when the source note is still actively used for planning, patrol, or roadmap navigation.
- Fix only when a canonical replacement target is known confidently.

#### 2. Legacy ADR-title / shorthand alias debt — real backlog, but mostly historical
The broken-link backlog is still dominated by old title-style and shorthand references, especially in `decisions/*` and `reference/*`.

Repeated families seen in current evidence:
- `Roadmap`
- full ADR-title aliases such as `ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning`
- title variants such as `ADR-023 Cognitive Memory Architecture`, `ADR-034 Agent Role Hierarchy`
- shorthand forms such as `ADR-006`, `ADR-009`, `ADR-014`, `ADR-022`

Classification:
- This is **real note-content debt**, not a memory-tool defect.
- Most of it should be treated as **legacy alias debt**, not as a fresh regression in current canonical notes.
- Do **not** broaden ADR-054 closure into mass historical ADR/reference normalization.

#### 3. Unresolved placeholder/prose link noise — opportunistic only
A small tail still looks like placeholder or prose text wrapped in wikilink syntax rather than intended canonical notes.

Representative examples from recent detail output:
- `wikilinks`
- `target`
- `Note Title`
- one-off prose labels in older notes

Classification:
- Leave this as **unresolved minor noise** unless the source note is otherwise being edited.
- No separate alias-resolution policy is recommended.

### Orphan buckets

#### 1. `reference/repo-maps/*` — intentional orphan-heavy generated inventory
Current orphan detail shows **67** repo-map notes under `reference/repo-maps/*`.

Classification:
- Treat as **expected baseline orphan noise**.
- These notes are generated/reference-oriented artifacts and should not trigger cleanup work by count alone.

#### 2. `cases/*` — tolerated retrieval-oriented inventory, not mass-link debt
Current orphan detail shows **271** orphan `cases/*` notes.

Classification:
- Treat this as **tolerated historical/search-first inventory** rather than generic emergency cleanup.
- A small subset may still deserve consolidation or linking when it becomes active epic knowledge, but the folder-wide count should not drive broad manual relinking.

#### 3. `reference/*` outside repo maps — mixed, often actionable when current
Current orphan detail shows **26** top-level `reference/*` notes outside repo maps.

Classification:
- This is a **mixed bucket**.
- Current reference notes used by planning/patrol flows are more actionable than historical repo maps or case extracts.
- These notes are the best candidate surface for any future narrow cleanup wave.

#### 4. `design/*` — small, active, likely actionable
Current orphan detail shows **2** orphan `design/*` notes in the sampled backlog.

Classification:
- Because design notes are usually active navigational surfaces, this is **actionable orphan debt**, not tolerated background inventory.
- The missing canonical ADR roadmap note problem belonged in this bucket; this roadmap note now materializes that surface.

#### 5. `research/*` — mostly archival
Current orphan detail shows only a small tail in `research/*` in the sampled output.

Classification:
- Prefer archival tolerance or later deprecation/consolidation over new backlinks unless a note is still an active canonical reference.

## Recommended next actions

### For ADR-054 closure
1. Keep `8vh1` focused on cleanup verification evidence.
2. Keep `lnvm` focused on final canonical memory-ref reconciliation.
3. Do **not** add a broad historical broken-link cleanup wave to ADR-054.

### For later memory-hygiene work outside this closure wave
1. If a follow-up cleanup task is opened, scope it to **current canonical notes only** — not to the entire historical ADR/reference backlog.
2. Limit broken-link fixes to:
   - confidently known canonical targets
   - active roadmap/reference/design notes whose navigation quality still matters
3. Treat broad `Roadmap` / ADR-title alias families as **legacy debt**, not as a mandatory closure blocker.

### Reporting / patrol recommendation
`memory_health()` should continue to report the **gross orphan count**, but patrol interpretation should bucket the backlog as:
- **Intentional/generated baseline:** `reference/repo-maps/*`
- **Tolerated retrieval inventory:** `cases/*`
- **Actionable active-note debt:** current `reference/*`, `design/*`, and any current canonical note with confident broken-link targets

Recommended policy:
- keep raw aggregate/detail behavior unchanged
- avoid suppressing orphan-heavy folders in the tool itself for now
- teach patrol/decomposition notes to classify those folders explicitly before opening cleanup work

## Closure guidance
ADR-054 can still close after `8vh1` and `lnvm` if those tasks verify cleanup evidence and reconcile final canonical refs. The larger residual broken-link/orphan backlog is now classified as mostly **post-closure memory-hygiene debt**, not ADR-054 implementation incompleteness.

## Relations
- [[decisions/adr-054-proposal-memory-artifact-hygiene-and-proactive-knowledge-curation]]
- [[reference/project-memory-broken-link-and-orphan-backlog-triage]]
- [[cases/classify-residual-broken-wikilinks-by-legacy-alias-type-before-cleanup]]
- [[cases/bucket-intentional-orphan-heavy-folders-separately-in-memory-health-reporting]]
- [[cases/broken-link-backlog-shifted-from-roadmap-artifact-to-legacy-shorthand-adr-title-aliases]]
