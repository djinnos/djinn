---
title: Working Spec — ADR-055 SQLite seam inventory
type: design
tags: ["working-spec","adr-055","cleanup"]
---

# Working Spec

## Active objective
- Track the ADR-055 SQLite migration seam inventory captured during roadmap/design work.
- Keep this as mutable planning context until the Dolt/MySQL migration wave decides which seams become durable canonical guidance.

## Relevant scope
- `.djinn/design`
- `server/src/db`
- `server/crates/djinn-db/src`

## Constraints
- This note is task-scoped working context promoted out of an extracted case because the original note only captured current migration inventory categories.
- The content is useful for ongoing ADR-055 implementation planning, but it is not yet a durable cross-task precedent.

## Current hypotheses
- Database bootstrap, migrations, lexical search, vector storage, and repository APIs are the highest-friction SQLite coupling buckets.
- The final durable notes should likely be a design or reference artifact rather than a historical case extract.

## Open questions
- Which seam inventory slices should become canonical design docs versus temporary migration checklists?
- When the ADR-055 rollout lands, should this note be promoted into a broader design/reference note or discarded?

## Captured session knowledge
The original extracted case observed that ADR-055 design work enumerated explicit SQLite-coupled surfaces so migration could proceed through known seams instead of scattered edits. The inventory was organized around bootstrap, migrations, lexical search, semantic vector storage, and repository APIs.

---
Created during ADR-054 cleanup on 2026-04-14 as the Working Spec replacement for [[cases/adr-roadmap-captured-sqlite-migration-seam-inventory-categories]].