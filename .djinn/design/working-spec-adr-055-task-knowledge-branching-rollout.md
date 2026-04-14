---
title: Working Spec — ADR-055 task knowledge branching rollout
type: design
tags: ["working-spec","adr-055","cleanup"]
---

# Working Spec

## Active objective
- Preserve the per-task knowledge-branching lifecycle captured during ADR-055 design work as mutable rollout context.
- Keep the wiring contract available while task dispatch, branch-scoped memory capture, and post-task promotion continue evolving.

## Relevant scope
- `.djinn/design`
- `server/crates/djinn-agent/src`
- `server/crates/djinn-db/src/repositories/note`

## Constraints
- The original extracted case described current architectural flow rather than a stable reusable precedent.
- This content should live as working context until the branching lifecycle settles into durable implementation guidance.

## Current hypotheses
- Task dispatch, session-memory writes, and promotion/cleanup hooks must remain an explicit lifecycle contract.
- Durable guidance should eventually live in canonical ADR/design material instead of an extracted historical case note.

## Open questions
- Which parts of the branching contract belong in enduring design docs versus temporary rollout coordination notes?
- What promotion and cleanup hooks still need to land before this can be rewritten as a durable case or pattern?

## Captured session knowledge
The original extracted case defined the concrete architectural flow for per-task knowledge branching so implementation could connect task dispatch, session-memory writes, and post-task promotion without re-deriving the lifecycle.

---
Created during ADR-054 cleanup on 2026-04-14 as the Working Spec replacement for [[cases/adr-055-integration-contract-for-per-task-knowledge-branching]]. Re-materialized canonically from task `019d89de-7e6b-7651-954f-cc325a0fcf22` so epic memory refs resolve through memory tools.
