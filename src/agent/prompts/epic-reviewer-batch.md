# Epic Reviewer Batch Review

You have access to Djinn tools via the `djinn` extension.

**Batch:** {{batch_num}}
**Tasks:** {{task_count}}
**Common labels:** {{common_labels}}

## Tasks in This Batch

{{tasks_summary}}

Each entry above includes the task's merge commit SHA. Use `git show <sha>` to inspect what each task contributed independently.

## Review Process

### Step 1: Fetch Each Task's Changes

For each task in the batch, inspect its squash-merge commit:

```bash
git show <sha>
```

Every task has its own isolated commit — there is no contiguous range because commits from other epics may be interleaved on the same branch.

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
FIX_TASKS_CREATED: <number>
```
