## Workflow E: Proposal Closeout Review

You have been dispatched on an `epic_breakdown` task because **every epic graduated from a proposal has now closed**. The proposal is still in `building`. Your job is to look at what was actually delivered and decide whether the proposal is *done* — then record that decision and stop. You operate one level *above* epics; there is **no `epic_id`** on this task, which is expected.

Your task `design` contains the proposal id.

### E1. Read the proposal and what it became

1. Call `proposal_show(id="<proposal-id-from-design>")`. Read the `title`, `body`, and especially `acceptance_criteria` — these are the bar the build must clear.
2. Review what the closed epics delivered. The targets are directly readable:
   - `read(project="owner/repo", file_path="...")` and `code_search(query="...", project="owner/repo")` against any target repo's default branch (where merged work now lives).
   - For the home project you also have `code_graph` and `build_context`.

### E2. Judge against the acceptance criteria

For each acceptance criterion, decide whether the merged work actually satisfies it. Be concrete — point at the code/files that fulfil it. A criterion that nothing addresses is a gap.

### E3. Decide — do exactly ONE

- **The proposal is satisfied** (every acceptance criterion is met by delivered work):
  call `proposal_complete(id="<proposal-id>", summary="<what shipped and how it meets the spec>")`.
  This marks the proposal `done`. You are finished.

- **Work remains** (one or more criteria are unmet, or follow-on work is required):
  create the additional epic(s) with `epic_create(..., proposal_id="<proposal-id>")` — set
  `read_sources`/`blocked_by` as in Workflow D — then call `submit_grooming(summary="...")`.
  The proposal stays `building`; once these new epics close you will be re-dispatched to review again.

Do **not** do both, and do **not** leave the task without doing one of them: if you neither complete
the proposal nor create new epics, the proposal will sit in `building` with nothing driving it forward.

Do NOT create worker tasks here, and do NOT set `next_patrol_minutes`.
