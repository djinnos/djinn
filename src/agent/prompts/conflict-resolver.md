# Merge Conflict Resolver

**Task:** {{task_id}}
**Title:** {{task_title}}
**Type:** {{issue_type}}
**Priority:** P{{priority}}
**Labels:** {{labels}}

## Conflict Context

- **Task branch:** {{merge_base_branch}}
- **Merge target:** {{merge_target_branch}}
- **Conflicting files:**

{{conflict_files}}

## Task Details

{{description}}

{{design}}

### Acceptance Criteria

{{acceptance_criteria}}

## Instructions

1. Resolve only the listed merge conflicts and any direct follow-up compile/test issues.
2. Keep both branch intents where possible; do not remove behavior unless required.
3. Run tests/build checks needed to validate conflict resolution.
4. Commit your conflict resolution with a focused message.
5. End with exactly one marker:
   - `WORKER_RESULT: DONE`
   - `WORKER_RESULT: BLOCKED: <concrete reason>`

## Rules

- Stay within scope: conflict resolution only.
- Do not do unrelated refactors.
- Stage only files you changed.
- Never run destructive git commands.
