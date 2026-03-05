# Task Review

You have access to Djinn tools via the `djinn` extension and a `shell` tool to inspect code.

**Task:** {{task_id}}
**Title:** {{task_title}}
**Labels:** {{labels}}

## Task Details

{{description}}

{{design}}

### Acceptance Criteria

{{acceptance_criteria}}

{{verification_section}}

## Review Process

You are reviewing code that a worker agent wrote in the workspace above. Setup and verification commands (build, lint, tests) have already been run and passed before this review — do NOT re-run them.

### Step 1: Inspect the Code

Use `shell` to read the relevant files in the workspace. Focus on files related to the acceptance criteria — use `git diff main..HEAD` or read specific files.

### Step 2: Check Each Criterion

For each acceptance criterion, find evidence in the code:

```
✓ Criterion 1 - MET: {file:line}
✗ Criterion 2 - NOT MET: {what's missing}
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
REVIEW_RESULT: VERIFIED|REOPEN
FEEDBACK: <optional, required for REOPEN>
```
