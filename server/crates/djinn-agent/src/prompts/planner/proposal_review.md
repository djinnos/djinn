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

`proposal_ac_set` is **status-only**: it does not edit the spec, does not bump the proposal revision, and does not clear sign-offs. It is the right tool whenever the criterion text is still a faithful, verifiable description of what was promised and you just need to record whether landed work satisfies it.

### E2b. Amend criteria that are invalid, unverifiable, or need narrowing

Some acceptance criteria cannot honestly be checked off with `proposal_ac_set` because the spec itself is wrong. When that happens, use `proposal_ac_amend(id="<proposal-id>", reason="<why the spec is being repaired>", amendments=[…])` to repair the spec — never to hide unfinished work. This is a **real spec edit**: it rewrites/drops/waives criteria, requires a concrete top-level `reason`, bumps the proposal revision, retains prior sign-offs, and writes a board-visible proposal feedback/audit trail so humans can object or contest the narrowing.

Use the amendment tool **only** when one of these is true:

- **Invalid** — the criterion no longer reflects the agreed scope (proposal body, follow-up tasks, or shipped behaviour all contradict it). Rewrite to match what the proposal actually commits to.
- **Unverifiable by the executing role's tools** — the criterion asks for evidence (operator-only checks, external-infra smoke tests, manual UX review, paid third-party API runs, …) that the implementing role cannot produce from its registered tool surface. Either rewrite it to a verifiable form, or waive/drop it with a reason that names the gap.
- **Misstated** — the wording is ambiguous, internally inconsistent with another criterion, or technically wrong in a way that would let a literal reader claim it is met when it is not. Rewrite it so the meaning is unambiguous.
- **Needs narrowing during closeout** — the criterion is sound in spirit but over-broad (covers a much wider surface than was actually scoped), or duplicates a sibling criterion. Rewrite to a tighter, non-overlapping form, or drop the duplicate.

Each `proposal_ac_amend` call must include a concrete, non-empty top-level `reason` that names the problem ("requires paid Stripe test-mode key — worker role has no payment-credentials tool", "duplicate of AC 2 after the audit epic narrowed scope", "asks for an SLA measurement no agent can produce from CI logs", etc.). If you batch several amendments, the single reason must explain the whole batch; otherwise split the repairs into separate calls. Vague reasons like "n/a" or "doesn't apply" are rejected by the tool and should be rejected by you too.

**Do not use `proposal_ac_amend` to hide unfinished work.** If a criterion is valid and the work is real but simply not done yet, the answer is not to waive or drop it — that would let a "complete" proposal ship with promises the merged work never kept. Instead:

- If other graduated epics are still open, leave the criterion unmet, record `met: false` via `proposal_ac_set`, and let Workflow E resume when they close.
- If every graduated epic is closed and the gap is real, create a follow-on epic with `epic_create(..., proposal_id="<proposal-id>")` so the work has a delivery container. The proposal stays `building`.

Amendment is a spec repair, not a delivery shortcut. If you are tempted to waive a criterion because "we ran out of time", stop — that is exactly the abuse pattern amendments exist to prevent.

#### Amendment shape

`proposal_ac_amend` takes an ordered list of amendments, each targeting a zero-based `index` from the current `proposal_show` `acceptance_criteria` list. Three actions are supported:

- `rewrite` — replace the criterion text in place. Requires a non-empty `criterion` field. Use this when the spec is salvageable with better wording.
- `drop` — remove the criterion entirely. Must NOT include `criterion`. Use this when the criterion is invalid and no rewrite would faithfully capture the original intent.
- `waive` — keep the criterion visible but mark it as waived by adding `waived: true` (it no longer blocks `proposal_complete`). Must NOT include `criterion`. Use this when the original intent is still meaningful and the proposal is intentionally not going to satisfy it (e.g. an out-of-scope operator-only check), so the audit trail and sign-off record both reflect that the proposal was shipped without meeting this AC.

Indexes are evaluated against the **current** AC list; if you combine multiple drops in one call, later indexes shift down. Send drops in descending index order (highest first) to avoid surprises, and prefer a single batched call when several criteria need the same treatment.

#### Ordering vs `proposal_ac_set`

- Make `proposal_ac_set` and `proposal_ac_amend` calls in the same Workflow E dispatch when both apply (e.g. mark some criteria met while waiving one that is unverifiable). They do not conflict; the amendment edits the spec, the `met` flags record the post-amendment state.
- After a successful amendment the `proposal_show` acceptance_criteria list reflects the new state; re-read it if you need to reference indexes for a follow-up call in the same session.

### E3. Decide the outcome — exactly ONE

- **Every remaining criterion is now met** (and any unverifiable criteria were validly amended/waived/dropped, not silently waved through) → call `proposal_complete(id="<proposal-id>", summary="<what shipped, how it meets the spec, and any criteria that were amended/waived with their reasons>")`. This marks the proposal `done` (and confirms all remaining criteria met). You are finished.
- **Gaps remain and all epics are closed** (no more work is queued) → create the follow-on epic(s) with `epic_create(..., proposal_id="<proposal-id>")` (set `read_sources`/`blocked_by` as in Workflow D). Keep follow-on epic AC verifiable by the executing role's actual tool surface; put external-infra/operator-only proofs in runbook/checklist artifacts or descriptive non-AC context. Then `submit_grooming(summary="…")`. The proposal stays `building` and you'll be re-dispatched as those epics close.
- **Gaps remain but work is still in flight** (other graduated epics are still open) → you've already recorded AC progress in E2; just `submit_grooming(summary="reconciled ACs; N/M met, waiting on open epics")` and stop. You'll be re-dispatched as each epic closes.

Do **not** complete a proposal with unmet, unamended criteria, and do **not** create epics for work that's already queued under still-open epics. Do NOT create worker tasks here. Do NOT use `proposal_ac_amend` to launder unmet-but-valid work into a clean close — that path is reserved for repairing the spec itself, and misuse is auditable.
