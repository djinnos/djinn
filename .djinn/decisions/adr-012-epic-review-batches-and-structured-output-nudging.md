---
title: ADR-012 Epic Review Batches and Structured Output Nudging
type: adr
tags: ["adr","review","execution"]
---

# ADR-012: Epic Review Batches and Structured Output Nudging

## Status
Accepted

## Context
Epic review currently relies on task statuses (`needs_epic_review`, `in_epic_review`) and per-task handoff timing. This mixes two concerns:
- delivery lifecycle for tasks
- aggregate quality review for an epic

We also observe reliability gaps when agents finish without required result markers (`WORKER_RESULT`, `REVIEW_RESULT`, `EPIC_REVIEW_RESULT`). In those cases we should retry with a targeted nudge before falling back.

## Decision
1. Task closure is not blocked by epic review.
   - After successful task review and merge, tasks close immediately.
2. Epic review is modeled as batch orchestration at epic level.
   - Epic enters `in_review` when all tasks are closed and there is reviewable delta.
   - Review work is tracked as explicit batches, not inferred from task status.
3. New tasks added during `in_review` or after epic closure reopen the epic to `open`.
4. Agent output handling uses bounded structured-output nudging for all roles.
   - If required marker is missing, send a targeted follow-up instruction in-session.
   - Apply a small retry budget, then fallback if marker still missing.
5. No system-generated follow-up tasks are created automatically when epic review reports issues.
   - The reviewer agent should create follow-up tasks in the same epic.
   - If it fails to do so, we preserve the batch verdict and leave epic open for manual/user handling.

## Consequences
### Positive
- Clearer separation of concerns: task delivery vs epic-level audit.
- Better restart/recovery and observability via persisted batch entities.
- Better agent reliability with marker-specific nudge retries.

### Trade-offs
- Additional schema/repository complexity for batch tracking.
- Requires strict tool-scoping enforcement so epic-review generated tasks stay in the same epic.

## Relations
- [[ADR-009: Simplified Execution — No Phases, Direct Task Dispatch]]
- [[V1 Requirements]]