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

### Step 4: Update Acceptance Criteria and Emit Verdict

1. **MANDATORY**: Call `task_update(id="{{task_id}}", acceptance_criteria=[...])` with every criterion set to `met: true` or `met: false`.
2. If any criterion is unmet, produce `REVIEW_RESULT: REOPEN` and include `FEEDBACK: <what is missing>`.
3. If all criteria are met, produce `REVIEW_RESULT: VERIFIED`.
4. If review cannot continue safely (missing diff/context), produce `REVIEW_RESULT: CANCEL` with `FEEDBACK: <reason>`.

Do not stop after analysis. You must emit a `REVIEW_RESULT` marker.

## Anti-Loop Reminder

- "Could be better" → VERIFY
- "I'd do differently" → VERIFY
- "Code smell" → VERIFY (phase reviewer's job)
- Criterion clearly unmet → REOPEN

**Default to VERIFY.**

---

## Output

After calling tools, provide a short review note with:

```
REVIEW_RESULT: VERIFIED|REOPEN|CANCEL
FEEDBACK: <optional, required for REOPEN/CANCEL>
```
