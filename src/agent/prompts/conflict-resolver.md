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

1. Resolve only the listed merge conflicts — fix conflict markers in the conflicting files.
2. Keep both branch intents where possible; do not remove behavior unless required.
3. Commit your conflict resolution with a focused message.

## Rules

- Stay within scope: conflict resolution only.
- Do not run build checks, `tsc`, tests, or linters — build validation is handled externally after your session.
- Do not do unrelated refactors.
- Stage only files you changed.
- Never run destructive git commands.
