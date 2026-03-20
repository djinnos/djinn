---
title: ADR-016: EpicReviewer Detached HEAD Worktree
type: adr
tags: ["adr","epic-review","worktree","sandbox"]
---


# ADR-016: EpicReviewer Detached HEAD Worktree

Status: Accepted
Date: 2026-03-04
Related: [[ADR-013: OS-Level Shell Sandboxing — Landlock + Seatbelt]]

## Context

With ADR-013, every agent session needs a well-defined worktree path for the
Landlock/Seatbelt sandbox policy. The write-allowed root must be known
server-side at dispatch time — not supplied by the agent.

Currently `EpicReviewer` sessions go through the same `prepare_worktree` as
worker sessions: a task branch (`task/{short_id}`) is created or rebased, and
a worktree is checked out on that branch. This is wrong for two reasons:

1. Epic reviewers read the aggregate diff; they don't commit work-in-progress.
   Checking out a task branch gives them a misleading working tree.
2. There is no "task short_id" context for an epic review batch — the batch has
   only a UUID (`EpicReviewBatch.id`), not a short_id.

## Decision

Add `prepare_epic_reviewer_worktree(project_dir, batch_id)` to the supervisor.
It creates a detached HEAD worktree at the current HEAD of the target branch:

```
git worktree add --detach .djinn/worktrees/batch-{batch_id} HEAD
```

- Folder name: `batch-{uuid}` — uses the full batch UUID, no short_id needed.
- Detached HEAD: no branch is created or modified. The reviewer sees the
  codebase as-is, which is the correct input for aggregate code review.
- Cleanup: same path as task worktrees — `git worktree remove` + prune on
  session end (success or failure).
- The resulting path is the write-allowed root for the Landlock/Seatbelt
  sandbox, consistent with ADR-013 policy.

The dispatch flow branches on `agent_type == EpicReviewer` before calling
`prepare_worktree`, delegating to `prepare_epic_reviewer_worktree` instead.

## Consequences

### Positive

- Epic reviewer sessions have a valid worktree path for sandbox enforcement.
- No spurious task branches created for epic review sessions.
- Worktree reflects actual HEAD state — correct input for aggregate review.
- Consistent cleanup lifecycle with task worktrees.

### Negative

- Additional code path in the supervisor dispatch flow.
- Detached HEAD worktrees are cleaned up on session end but not on an
  unexpected server crash — reconciliation on startup must handle stale
  `batch-*` worktrees (same as existing stale worktree detection).

## Relations

- [[ADR-013: OS-Level Shell Sandboxing — Landlock + Seatbelt]]
- [[OS Shell Hardening Scope]]
