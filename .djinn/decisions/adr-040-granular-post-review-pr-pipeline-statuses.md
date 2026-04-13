---
title: ADR-040: Granular Post-Review PR Pipeline Statuses
type: adr
tags: ["adr","architecture","task-lifecycle","pr-workflow"]
---



# ADR-040: Granular Post-Review PR Pipeline Statuses

**Status:** Accepted
**Date:** 2026-03-22
**Related:** [[ADR-037: GitHub App PR Workflow and CI-Based Verification]], [["ADR-039: Replace Clerk and GitHub App with GitHub OAuth App"]]

---

## Context

### Reviewer is coupled to merge

The current flow has the reviewer synchronously attempt PR creation/merge during its session. If GitHub credentials are invalid or the API is down, the task escalates to `needs_lead_intervention` — a dead end that requires an agent to manually intervene. There's no retry mechanism for infrastructure failures.

Observed on 2026-03-22: reviewer approved task `nzho`, all 3 AC met, verification passed. PR creation failed because the GitHub OAuth App token refresh logic was broken (`incorrect_client_credentials`). Task escalated to lead, who force-closed it. The actual code changes were done — only the PR couldn't be created.

### No visibility into PR lifecycle

The single `pr_ready` status conflates three distinct states:
1. PR created as draft, CI running
2. PR undrafted, awaiting human review
3. PR approved, awaiting merge

Users can't tell from the board whether a task is waiting on CI or waiting on a human.

### Credential failures are not retryable

When PR creation fails due to missing/invalid credentials, the task gets stuck. There's no status where the coordinator can simply retry after the user fixes their credentials.

---

## Decision

### Replace `pr_ready` with three new statuses

| Status | Meaning | Entry | Exit |
|--------|---------|-------|------|
| `approved` | Reviewer/Lead approved. No PR yet. | `TaskReviewApprove` or `LeadApprove` | PR created → `pr_draft`. Merge conflict → `open`. Stays here on cred failure (retryable). |
| `pr_draft` | PR exists as draft. CI running. | Coordinator creates PR successfully | CI passes → `pr_review`. CI fails → `open`. Merge conflict → `open`. |
| `pr_review` | PR undrafted. Awaiting human review. | CI passed, PR undrafted by poller | Human approves + merged → `closed`. Changes requested → `open`. |

### Decouple reviewer from merge

The reviewer's only job on approval is to transition the task to `approved`. It does NOT attempt PR creation, branch pushing, or merging. The coordinator picks up `approved` tasks on its next tick and handles the PR pipeline.

### Lead can also approve to `approved`

The lead's `LeadApprove` action also transitions to `approved` (not directly to `closed`). This gives the lead the same PR pipeline benefits — retryable credential failures, CI gating, human review.

### `approved` is a retryable holding state

If PR creation fails (credential error, API error, rate limit), the task stays in `approved`. The coordinator retries on subsequent ticks. Users fix credentials at their leisure — no agent intervention needed.

If PR creation fails due to a merge conflict, the task transitions back to `open` for the worker to resolve.

### Direct-push fallback removed from reviewer/lead

When no GitHub OAuth credential exists, the task still moves to `approved`. The coordinator detects the missing credential and either:
- Performs a direct squash-merge (no PR) and transitions to `closed`
- Or stays in `approved` waiting for the user to connect GitHub

This is a project-level configuration decision, not a per-task one.

---

## New Transition Actions

| Action | From | To | Notes |
|--------|------|----|-------|
| `TaskReviewApprove` | `in_task_review` | `approved` | Changed: was → `closed` |
| `LeadApprove` | `in_lead_intervention` | `approved` | Changed: was → `closed` |
| `PrCreated` | `approved` | `pr_draft` | New |
| `PrUndraft` | `pr_draft` | `pr_review` | New |
| `PrCiFailed` | `pr_draft` | `open` | New: CI check failed |
| `PrChangesRequested` | `pr_review` | `open` | Existing, source changed |
| `PrMerge` | `pr_review` | `closed` | Existing, source changed |
| `PrConflict` | `approved` or `pr_draft` | `open` | New: merge conflict detected |

---

## Consequences

### Positive
- Credential failures are self-healing — fix creds, coordinator retries
- Clear board visibility: is the task waiting on CI, human, or creds?
- Reviewer sessions are faster — no synchronous merge attempt
- Lead approvals get the same PR pipeline (CI gate, human review)
- No more force-closing tasks that were successfully implemented

### Negative
- More statuses to track (9 → 11 active statuses)
- Coordinator needs a new dispatch path for `approved` tasks
- PR poller needs to handle `pr_draft` and `pr_review` separately

---

## Relations

- [[ADR-037: GitHub App PR Workflow and CI-Based Verification]]
- [["ADR-039: Replace Clerk and GitHub App with GitHub OAuth App"]]
- [[roadmap]]
