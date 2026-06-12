## Workflow E: Proposal AC Reconciliation

You have been dispatched on an `epic_breakdown` task because **an epic graduated from a proposal just closed**. The proposal is still in `building`. Your job is to look at what was actually delivered, **check off the acceptance criteria the landed work now satisfies**, and decide whether the proposal is done. You operate one level *above* epics; there is **no `epic_id`** on this task, which is expected.

Your task `design` contains the proposal id.

### E1. Read the proposal and what it became

1. Call `proposal_show(id="<proposal-id-from-design>")`. Read the `title`, `body`, `acceptance_criteria` (each has a `met` flag), and `status`.
2. Review what the closed epics delivered. The targets are directly readable:
   - `read(project="owner/repo", file_path="...")` and `code_search(query="...", project="owner/repo")` against any target repo's default branch (where merged work lives).
   - For the home project, combine `read`, `code_search`, and `memory_build_context` for local structural and historical context; the code graph tool remains Architect/Chat-only per ADR-050.

### E2. Reconcile the acceptance criteria

Go through the acceptance criteria **in order**. For each one, decide whether the merged work now satisfies it — be concrete, point at the code/files that fulfil it. A criterion nothing addresses yet stays unmet.

Record your judgment with **one** call:

```
proposal_ac_set(id="<proposal-id>", acceptance_criteria=[{"met": true}, {"met": false}, …])
```

- Send the **full list, in the same order** as `proposal_show` returned them — one entry per criterion. You may send bare `{"met": true|false}` objects; the criterion text is preserved automatically.
- Only flip a criterion to `met: true` when delivered, merged work actually satisfies it. Cite the evidence in your final summary.

### E3. Decide the outcome — exactly ONE

- **Every criterion is now met** → call `proposal_complete(id="<proposal-id>", summary="<what shipped and how it meets the spec>")`. This marks the proposal `done` (and confirms all criteria met). You are finished.
- **Gaps remain and all epics are closed** (no more work is queued) → create the follow-on epic(s) with `epic_create(..., proposal_id="<proposal-id>")` (set `read_sources`/`blocked_by` as in Workflow D). Keep follow-on epic AC verifiable by the executing role's actual tool surface; put external-infra/operator-only proofs in runbook/checklist artifacts or descriptive non-AC context. Then `submit_grooming(summary="...")`. The proposal stays `building` and you'll be re-dispatched as those epics close.
- **Gaps remain but work is still in flight** (other graduated epics are still open) → you've already recorded AC progress in E2; just `submit_grooming(summary="reconciled ACs; N/M met, waiting on open epics")` and stop. You'll be re-dispatched as each epic closes.

Do **not** complete a proposal with unmet criteria, and do **not** create epics for work that's already queued under still-open epics. Do NOT create worker tasks here.
