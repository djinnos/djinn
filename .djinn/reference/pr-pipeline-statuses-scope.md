---
title: PR Pipeline Statuses Scope
type: reference
tags: ["scope","reference","pr-workflow"]
---

# PR Pipeline Statuses Scope

## In Scope

### 1. Task State Machine Changes (`server/crates/djinn-core/src/models/task.rs`)
- Add `Approved`, `PrDraft`, `PrReview` statuses
- Remove `PrReady` status (replaced by `PrDraft` + `PrReview`)
- Add transition actions: `PrCreated`, `PrUndraft`, `PrCiFailed`, `PrConflict`
- Change `TaskReviewApprove` target: `in_task_review` → `approved` (was → `closed`)
- Change `LeadApprove` target: `in_lead_intervention` → `approved` (was → `closed`)
- Update `PrMerge` source: `pr_review` (was `pr_ready`)
- Update `PrChangesRequested` source: `pr_review` (was `pr_ready`)
- DB migration for new status enum values

### 2. Decouple Reviewer from Merge (`server/crates/djinn-agent/src/roles/reviewer.rs`, `task_merge.rs`)
- Reviewer `on_complete()`: on approval, just transition to `approved` — do NOT call `merge_after_task_review()`
- Remove or refactor `merge_after_task_review()` — merge logic moves to coordinator
- Lead extension `lead_approve`: transition to `approved` — do NOT call `merge_and_transition()`

### 3. Coordinator PR Dispatch (`server/crates/djinn-agent/src/actors/coordinator/`)
- New dispatch path: pick up tasks in `approved` status
- Attempt PR creation (push branch + create draft PR via GitHub API)
- On success: transition `approved` → `pr_draft`
- On credential failure: log warning, leave in `approved` (retry next tick)
- On merge conflict: transition `approved` → `open` with conflict metadata
- No-credential fallback: direct squash-merge → `closed` (when GitHub not configured)

### 4. PR Poller Updates (`server/crates/djinn-agent/src/actors/coordinator/pr_poller.rs`)
- Monitor `pr_draft` tasks: check CI status
  - CI passes + no conflicts → undraft PR, transition `pr_draft` → `pr_review`
  - CI fails → transition `pr_draft` → `open` with CI failure details
  - Merge conflict → transition `pr_draft` → `open`
- Monitor `pr_review` tasks: check review status
  - Changes requested → transition `pr_review` → `open` with review feedback
  - Approved + merged → transition `pr_review` → `closed`
  - PR closed without merge → `ForceClose`

### 5. MCP Tool Updates
- `task_show` displays new statuses correctly
- `task_list` filters work with new statuses
- `board_health` accounts for new statuses

### 6. Tests
- Update exhaustive state machine transition tests in `server/crates/djinn-agent/src/extension/`
- Update task lifecycle tests
- Update PR poller tests
- Update reviewer role tests
- Update snapshot tests if affected

## Out of Scope
- Desktop UI changes (frontend will need updates but not in this scope)
- New MCP tools for PR management
- Webhook-based PR monitoring (staying with polling per ADR-037)
- Changes to verification pipeline
- Changes to groomer/planner flow

## Preferences
- Keep the migration backward-compatible: existing `pr_ready` tasks should map to `pr_draft`
- Coordinator PR dispatch should be a separate function, not inline in the dispatch tick
- Log all status transitions with reasons for debugging

## Relations
- [[ADR-040: Granular Post-Review PR Pipeline Statuses]]
- [[ADR-037: GitHub App PR Workflow and CI-Based Verification]]
- [[Roadmap]]
