# Epic Reviewer Batch Review

You have access to Djinn tools via the `djinn` extension.

**Batch:** {{batch_num}}
**Tasks:** {{task_count}}
**Commits:** {{commit_count}}
**Commit range:** {{start_commit}}..{{end_commit}}
**Common labels:** {{common_labels}}

## Tasks in This Batch

{{tasks_summary}}

## Review Process

### Step 1: Fetch Code Changes

Run these commands to see what was changed:

```bash
git diff {{start_commit}}..{{end_commit}}
git log --oneline {{start_commit}}..{{end_commit}}
```

### Step 2: Architectural Review

For each task's changes, check:
- Does the code follow established patterns?
- Are there architectural violations?
- Any cross-cutting concerns missed (error handling, logging, security)?
- Code duplication across tasks that should be refactored?

### Step 3: Integration Review

Check how changes from different tasks interact:
- Any conflicting patterns or approaches?
- Shared state handled correctly?
- API contracts consistent?

### Step 4: Create Fix Tasks (if needed)

If issues are found, create fix tasks using `task_create`:

```
task_create(
    title="Fix: {description of issue}",
    issue_type="task",
    project="{{project_path}}",
    description="{what needs to be fixed and why}",
    labels=[{{common_labels}}]
)
```

## Output

Emit exactly one status marker:

```
ARCHITECT_BATCH_RESULT: CLEAN
```
or
```
ARCHITECT_BATCH_RESULT: ISSUES_FOUND
```

Also include:

```
BATCH_NUMBER: {{batch_num}}
TASKS_REVIEWED: {{task_count}}
COMMITS_REVIEWED: {{commit_count}}
FIX_TASKS_CREATED: <number>
```
