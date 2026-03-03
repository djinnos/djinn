# Task Review

You have access to Djinn tools via the `djinn` extension.

**Task:** {{task_id}}
**Title:** {{task_title}}
**Commit range:** {{start_commit}}..{{end_commit}}
**Labels:** {{labels}}

## Task Details

{{description}}

{{design}}

### Acceptance Criteria

{{acceptance_criteria}}

## Commits

```
{{commits}}
```

## Diff

```diff
{{diff}}
```

## Review Process

### Step 1: Extract Acceptance Criteria

From task details above, create checklist:

```
□ Criterion 1
□ Criterion 2
□ Criterion 3
```

### Step 2: Check Each Against Diff

For each criterion, find evidence in the diff above:

```
✓ Criterion 1 - MET: {file:line}
✓ Criterion 2 - MET: {test proves behavior}
✗ Criterion 3 - NOT MET: {what's missing}
```

### Step 3: Red Team / Blue Team

**Red Team** - For unclear/unmet criteria:
- What evidence is missing?
- Is there a gap between asked and delivered?

**Blue Team** - Challenge each finding:
- Is this ACTUALLY required by criteria as written?
- Am I adding scope that wasn't requested?
- Is this "not how I'd do it" vs "not done"?

**Rule:** If Blue Team has ANY reasonable defense → DROP the finding

### Step 4: Update Acceptance Criteria and Decide

**MANDATORY: You MUST call `task_update` with acceptance_criteria BEFORE emitting REVIEW_RESULT.** Skipping this step is a review failure.

Call `task_update(id="{{task_id}}", acceptance_criteria=[...], project="{{project_path}}")` with every criterion set to `met: true` or `met: false`.

**Adding new criteria:** If you found issues NOT covered by the original criteria (e.g., code doesn't compile, wrong file location), add them as new entries with `met: false`.

**Then, and ONLY after the task_update call succeeds**, emit your verdict:

| All MET | → VERIFY |
| Any NOT MET | → REOPEN |
| No meaningful changes required | → CANCEL |

## Anti-Loop Reminder

- "Could be better" → VERIFY
- "I'd do differently" → VERIFY
- "Code smell" → VERIFY (phase reviewer's job)
- Criterion clearly unmet → REOPEN

**Default to VERIFY.**

---

## Output

**VERIFIED:**
```
REVIEW_RESULT: VERIFIED
```

**REOPEN:**
```
REOPEN_REASON: {criterion} not met. Missing: {what}
REVIEW_RESULT: REOPEN
```

**CANCEL:**
```
CANCEL_REASON: {why this task should be canceled}
REVIEW_RESULT: CANCEL
```
