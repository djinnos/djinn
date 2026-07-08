# Learned-Prompt Harvest Artifact & Operator Runbook

> **Epic:** Harvest active learned prompts and preserve prompt-equivalence evidence (`t8p8`)
> **Proposal:** `z5f9` — Remove the learned-prompt auto-coaching subsystem
> **Roadmap:** `design/t8p8-roadmap`
> **Owner (worker template):** epic `t8p8`
> **Gate:** This artifact must be present, signed off, and the disposition table complete
> before any downstream runtime, schema, MCP/REST, or UI removal of the learned-prompt
> subsystem may land. It is the reviewer-gate evidence for sibling epics
> (`3x0w`, `3sle`, `8m3c`).

## 1. Purpose & scope

The learned-prompt subsystem currently mutates the system prompt at runtime. Two
runtime facts govern the harvest contract:

- The "active" learned-prompt set for an agent is derived from rows in
  `learned_prompt_history` whose `action IN ('keep','confirmed')`, appended in
  `created_at ASC` order with the literal separator `E'\n\n---\n\n'`
  (`server/crates/djinn-db/src/repositories/agent.rs`, all six `string_agg`
  call sites under `SELECT … FROM learned_prompt_history h … AND h.action IN ('keep','confirmed')`).
- Prompt assembly currently appends the derived text **after**
  `system_prompt_extensions` in
  `server/crates/djinn-agent/src/prompts.rs::apply_role_extensions`, with the
  order `base rendered prompt → system_prompt_extensions → learned_prompt`.

This artifact is the canonical, repository-side evidence path for inventorying
those active rows, recording one disposition per row, and capturing the
prompt-equivalence comparison required to confirm that no behavioral drift is
introduced by the eventual removal. **It is additive documentation only.** It
does not propose, draft, or apply a drop migration, and it does not remove any
runtime, API, MCP, REST, or UI code. All destructive work is owned by sibling
epics.

### 1.1 Non-goals (mandatory)

- **A worker environment must not fabricate a production or staging harvest.**
  Capturing the inventory query result, populating the disposition table,
  computing the checksum, or recording a reviewer sign-off is an operator /
  deployment-checklist responsibility that requires direct read-only access to
  the target environment's database. The worker task that authors this file
  only commits the template, the exact inventory query, and the operator
  runbook. Any field below that is "operator-supplied" must remain `TBD` until
  an operator fills it in against the live environment.
- **No drop migration, no schema removal, no runtime/API/UI deletion is allowed
  in any task that authors or fills in this artifact.** Those removals are
  owned by sibling epics `3x0w` (runtime/repo/MCP/REST/prompt-assembly paths),
  `3sle` (generated artifacts + UI surfaces), and `8m3c` (schema drop + design
  records). This epic only establishes the harvest evidence path that gates
  those removals.
- **No sign-off, row count, checksum, or environment name may be written by a
  worker task.** These are operator/deployment checklist items. If a field is
  missing, the artifact is intentionally incomplete and downstream destructive
  work must be blocked.

## 2. Environment & timestamp

These fields are **operator-supplied**. The worker must not populate them.

| Field | Value | Supplied by |
|---|---|---|
| **Environment name** | `TBD` (e.g. `production`, `staging`, `staging-eu`, `prod-us-east`) | Operator on target environment |
| **Database host / connection** | `TBD` (use a read-only role; never `postgres` superuser) | Operator on target environment |
| **Database name** | `TBD` | Operator on target environment |
| **Harvest run timestamp (UTC)** | `TBD` (ISO-8601, e.g. `2026-MM-DDTHH:MM:SSZ`) | Operator at run time |
| **Operator name / handle** | `TBD` | Operator at run time |
| **Tooling version** | `TBD` (e.g. `psql 16.x`, or the active-row export helper shipped by sibling task `iykf`) | Operator at run time |
| **Cutover target window** | `TBD` (the window during which the destructive removal is planned) | Deployment owner |

### 2.1 Pre-flight checklist (operator)

- [ ] Connected to the **target** environment with a read-only role.
- [ ] Confirmed `learned_prompt_history` is present and the schema matches
      `server/crates/djinn-db/migrations_postgres/1_initial_schema.sql`
      (columns: `id`, `agent_id`, `proposed_text`, `action`, `metrics_before`,
      `metrics_after`, `created_at`).
- [ ] Confirmed `agents.learned_prompt` is the stale text column and that
      `string_agg(h.proposed_text, E'\n\n---\n\n' ORDER BY h.created_at ASC)`
      is the live aggregation used by the application
      (`server/crates/djinn-db/src/repositories/agent.rs`).
- [ ] Captured the harvest timestamp in UTC ISO-8601.
- [ ] Saved the raw export and any helper output to a path that will be
      referenced in §4.

## 3. Active-row inventory query

Run the following query verbatim against the target environment's database
(read-only role). The query is the **only** inventory source for this artifact.
Do not paraphrase it, do not add `LIMIT`, and do not re-order the result.

```sql
SELECT a.project_id, a.id AS agent_id, a.name AS agent_name, lph.action, lph.created_at, lph.amendment FROM agents a JOIN learned_prompt_history lph ON lph.agent_id = a.id WHERE lph.action IN ('keep','confirmed') ORDER BY a.project_id, a.id, lph.created_at ASC;
```

### 3.1 Why this exact query

- It joins through `learned_prompt_history` (the source of truth for the
  derived `learned_prompt` value), not the stale `agents.learned_prompt` text
  column.
- It restricts to the runtime-active set
  (`action IN ('keep','confirmed')`), matching the predicate the application
  uses to build the live `learned_prompt` value.
- It orders by `a.project_id, a.id, lph.created_at ASC` so the inventory is
  deterministic across runs, environments, and reviewers. This is the same
  order the application uses for the `string_agg(... ORDER BY h.created_at ASC)`
  aggregation, so the inventory is a faithful enumeration of what the runtime
  will append to each role's prompt.

### 3.2 Capture instructions

1. Pipe the query output to a TSV file with stable column ordering. The
   canonical columns are, in order:
   `project_id`, `agent_id`, `agent_name`, `action`, `created_at`, `amendment`.
2. Record the export path in §4.
3. Compute the SHA-256 checksum of the raw export and record it in §4.
4. Record the total row count returned in §4.

If a sibling task (`iykf`) ships an export/checksum helper, prefer that helper
and record the helper version in §2 alongside the tooling version. The helper
**must** execute the exact query above — any deviation invalidates the
inventory.

## 4. Row count, checksum & export reference

| Field | Value | Supplied by |
|---|---|---|
| **Active-row count** (rows returned by §3 query) | `TBD` | Operator |
| **Export file path** | `TBD` (e.g. an internal artifact bucket or controlled path) | Operator |
| **Export format** | `TBD` (TSV with the six columns above, header row included) | Operator |
| **Export SHA-256 checksum** | `TBD` (hex, lower-case) | Operator |
| **Export size (bytes)** | `TBD` | Operator |
| **Distinct project_id count** | `TBD` | Operator |
| **Distinct (project_id, agent_id) count** | `TBD` | Operator |
| **Breakdown by `action`** | `keep`: `TBD` / `confirmed`: `TBD` | Operator |
| **Inventory query tool / version** | `TBD` (or sibling helper `iykf` version) | Operator |
| **Run start time (UTC)** | `TBD` | Operator |
| **Run end time (UTC)** | `TBD` | Operator |
| **Reproducibility note** | Re-running the query against the same snapshot MUST yield the identical row count and checksum. If it does not, halt and treat the harvest as failed. | Operator |

### 4.1 Empty-inventory path

If the active-row count is `0`, that is a valid state (no project has any
`keep`/`confirmed` amendments at harvest time). In that case:

- The disposition table in §5 collapses to a single explanatory row noting the
  empty inventory.
- The reviewer sign-off (§7) still must be recorded before destructive
  removal, but it explicitly states that no preservation was required.
- The prompt-equivalence comparison (§6) is still run as a null-case baseline
  (both pre- and post-cutover renders of every role should be identical because
  the learned overlay contributed nothing).

## 5. Disposition table

One row per active prompt row from §3. The columns are exactly:

- `project_id`, `agent_id`, `agent_name`, `action`, `created_at` — copied
  verbatim from the §3 query output (the operator's source of truth for the
  unique key of each row).
- `disposition` — one of the four allowed values:
  - `fold into base prompt` — the amendment text is promoted into the
    hand-authored base prompt (e.g. a role template under
    `server/crates/djinn-roles/src/prompts/`). Requires a semantic-rationale
    block in §6.2 because the change is a global prompt-engineering edit, not
    a byte-preserving move.
  - `fold into project/role system_prompt_extensions` — the amendment text is
    appended to the agent's `system_prompt_extensions` JSONB column. Requires
    a byte-equivalence block in §6.1 because the runtime concatenation order
    `base → system_prompt_extensions → learned_prompt` means the byte sequence
    seen by the model is preserved when the text is moved into
    `system_prompt_extensions` *and* the moved text is appended at the
    position previously held by `learned_prompt`.
  - `convert to memory note` — the amendment text is preserved as a project or
    role memory note (see `server/crates/djinn-db` note tables). The agent's
    prompt no longer contains the text; the rationale is documented in §6.3.
  - `discard` — the amendment is intentionally dropped. The rationale
    (stale, duplicate, contradictory, or low-value) is recorded in §6.3.
- `destination` — for `fold into base prompt` and
  `fold into project/role system_prompt_extensions`, the exact destination
  reference (file path + section, or `system_prompt_extensions` JSONB key +
  ordering); for `convert to memory note`, the note table + key; for `discard`,
  `n/a`. **Every preserved row must have a non-empty `destination`.**
- `rationale reference` — pointer into §6 for the supporting evidence.

| # | project_id | agent_id | agent_name | action | created_at | disposition | destination | rationale reference |
|---|---|---|---|---|---|---|---|---|
| 1 | `TBD` | `TBD` | `TBD` | `TBD` | `TBD` | `TBD` (one of the four allowed values) | `TBD` (non-empty for any preserved row) | `TBD` (pointer to §6.1 / §6.2 / §6.3) |

> The operator inserts one row per active row from §3, in the same order as the
> §3 query output. The reviewer sign-off (§7) must confirm row count parity:
> disposition rows = active-row count from §4.

### 5.1 Disposition rules

- **No silent drops.** A `discard` must be justified in §6.3.
- **No double-preservation.** A single amendment text may be split across
  multiple destinations (e.g. one sentence promoted to the base prompt and
  one sentence moved to memory), in which case the disposition table records
  one row per split fragment with the same `(project_id, agent_id, created_at)`
  and a clearly distinguished `destination`. The original row's full text
  remains authoritative in the §3 export.
- **No new writes to `learned_prompt_history`.** The harvest is read-only. If
  the operator discovers a need to write (e.g. to mark a row discarded), that
  is out of scope for this artifact and must be tracked as a separate
  operational task against the live environment, not as a worker session edit.

## 6. Prompt-equivalence evidence

This section is the bridge between the disposition table and the eventual
removal: it must show that, after disposition, the **assembled system prompt
the model sees** for every preserved row is either byte-identical to the
pre-cutover assembled prompt, or has an explicit semantic rationale that
reviewers accept. Two evidence regimes apply.

### 6.1 Byte-equivalence for `system_prompt_extensions` moves

For every row with disposition `fold into project/role system_prompt_extensions`,
the operator must capture and compare the assembled system prompt **before** the
destructive removal and **after** the move, with the following rules.

**Pre-cutover capture (against the live environment, pre-removal):**

- For the affected `(project_id, agent_id)`, render the assembled system prompt
  using the production code path that calls
  `apply_role_extensions(base, system_prompt_extensions, learned_prompt)` in
  `server/crates/djinn-agent/src/prompts.rs`, with `learned_prompt` set to the
  exact `string_agg(h.proposed_text, E'\n\n---\n\n' ORDER BY h.created_at ASC)`
  result for that agent.
- Record the rendered prompt and its SHA-256 checksum.

**Post-cutover capture (against the post-move state):**

- For the same `(project_id, agent_id)`, render the assembled prompt with
  `learned_prompt` set to `None` and the moved amendment text appended to
  `system_prompt_extensions` in the same trailing position previously held by
  `learned_prompt`.
- Record the rendered prompt and its SHA-256 checksum.

**Equivalence check:**

- The two SHA-256 checksums **must** match. The runtime concatenation order
  (`base → system_prompt_extensions → learned_prompt`) is preserved when the
  text is moved into `system_prompt_extensions` and appended at the same
  trailing position, so byte identity is the expected and required outcome.
- If the checksums do not match, the move is **not** byte-equivalent and the
  disposition must be re-classified (likely to `fold into base prompt` with a
  semantic rationale, or to `convert to memory note` / `discard` with a
  documented drift).

**Evidence fields (per preserved row):**

| Field | Value |
|---|---|
| `(project_id, agent_id)` | `TBD` |
| Disposition | `fold into project/role system_prompt_extensions` |
| `system_prompt_extensions` destination (key/ordering) | `TBD` |
| Pre-cutover assembled-prompt SHA-256 | `TBD` |
| Pre-cutover assembled-prompt length (chars) | `TBD` |
| Post-cutover assembled-prompt SHA-256 | `TBD` |
| Post-cutover assembled-prompt length (chars) | `TBD` |
| Checksum match? | `TBD` (`yes` / `no`) |
| Capture tool / version | `TBD` (or sibling helper from `iykf` / `0l78`) |
| Reviewer note | `TBD` |

### 6.2 Semantic rationale for `base prompt` promotions

For every row with disposition `fold into base prompt`, byte identity is **not**
expected and is **not required. The expected outcome is a semantic rationale**
that explains:

- **What** the amendment text adds to the base prompt (one or two sentences
  summarizing the new behavioral instruction).
- **Why** it belongs in the hand-authored base prompt rather than in
  per-project/role `system_prompt_extensions` (e.g. it is a general
  improvement that applies to every project using this role, not a
  project-local refinement; or it corrects a defect in the base prompt that
  all projects inherit).
- **Where** the text was placed in the base prompt (file path, section
  heading, line range) and **how** it reads in context (quote the surrounding
  paragraph).
- **Risk** of behavioral drift versus the pre-cutover state (e.g. "same
  instruction, now applies to all projects using the worker role, not just
  project X" — acceptable; "rewording changes the model interpretation in
  cases Y" — unacceptable; require a new disposition).
- **Reviewer** who accepted the semantic rationale.

If a single amendment is promoted into the base prompt and the operator
chooses to rewrite the wording for prompt-voice consistency, the original
amendment text must still be preserved verbatim in the §3 export and quoted
in this section so reviewers can compare the pre- and post-cutover wording.

### 6.3 Memory notes and discards

For every row with disposition `convert to memory note` or `discard`, record:

- The full amendment text (copy from the §3 export).
- The destination (note table + key) for `convert to memory note`, or `n/a` for
  `discard`.
- The rationale: why the text is preserved as a memory note rather than a
  prompt extension, or why it is discarded (stale, duplicate, contradictory,
  off-policy, low-value, etc.).

## 7. Reviewer sign-off (gates destructive migration)

This section **must be completed by an operator or deployment owner against the
target environment**. A worker environment must not record, fabricate, or
counter-sign this section. Downstream destructive work (sibling epics `3x0w`,
`3sle`, `8m3c`) is **blocked** until every box below is checked and signed.

### 7.1 Pre-conditions

- [ ] §3 inventory query was executed against the **target** environment by an
      operator with read-only DB access.
- [ ] §4 row count, checksum, and export reference are populated and the
      inventory is reproducible.
- [ ] §5 disposition table has exactly one row per active row from §3, with
      allowed dispositions only and a non-empty `destination` for every
      preserved row.
- [ ] §6.1 byte-equivalence captures are recorded and checksum-matched for
      every `fold into project/role system_prompt_extensions` row, **or** the
      row has been re-classified with an updated rationale.
- [ ] §6.2 semantic rationale is recorded and accepted for every
      `fold into base prompt` row.
- [ ] §6.3 rationale is recorded for every `convert to memory note` and
      `discard` row.
- [ ] No new rows were inserted into `learned_prompt_history` during the
      harvest. (Re-run the §3 query after the export and confirm the row
      count and checksum are unchanged. If they changed, re-do the harvest.)

### 7.2 Sign-off

| Field | Value |
|---|---|
| **Environment name** | `TBD` (must match §2) |
| **Harvest run timestamp (UTC)** | `TBD` (must match §2) |
| **Active-row count (signed)** | `TBD` (must match §4) |
| **Export SHA-256 (signed)** | `TBD` (must match §4) |
| **Reviewer name / handle** | `TBD` |
| **Reviewer role** | `TBD` (e.g. deployment owner, on-call lead, release manager) |
| **Review date (UTC)** | `TBD` |
| **Reviewer statement** | `TBD` (e.g. "I have reviewed the §3 inventory, §5 disposition, and §6 prompt-equivalence evidence for `<environment>` harvested at `<timestamp>`. The dispositions and evidence are complete and acceptable. I authorize the destructive learned-prompt removal to proceed against this environment under sibling epics `3x0w`, `3sle`, `8m3c`.") |
| **Reviewer signature** | `TBD` (e.g. signed commit hash, SSO handle, or attached signed-off-by line) |

### 7.3 What this sign-off does **not** authorize

- It does **not** authorize the worker session to perform destructive work.
  Worker sessions that have authored or edited this artifact must not run
  schema drops, runtime removals, or API/UI deletions.
- It does **not** retroactively sign off harvests run from a worker
  environment. If a worker session has populated any field above, the
  sign-off is invalid and must be re-recorded by an operator against a fresh
  harvest.
- It is environment-scoped. A sign-off for `staging` does not authorize the
  same destructive change against `production`. A separate harvest and
  sign-off is required per environment.

## 8. References

- `server/crates/djinn-db/src/repositories/agent.rs` — runtime active
  predicate `action IN ('keep','confirmed')` and `string_agg` ordering.
- `server/crates/djinn-agent/src/prompts.rs::apply_role_extensions` — runtime
  assembly order `base → system_prompt_extensions → learned_prompt`.
- `server/crates/djinn-db/migrations_postgres/1_initial_schema.sql` — schema
  for `agents` and `learned_prompt_history`.
- `design/t8p8-roadmap` — epic roadmap; lists wave 1 task ordering.
- Proposal `z5f9` — Remove the learned-prompt auto-coaching subsystem.
- Sibling epics (downstream, blocked by this artifact): `3x0w` (runtime
  removal), `3sle` (UI/generated artifacts), `8m3c` (schema drop).
- Sibling task `iykf` — active-row inventory export and checksum helper
  (preferred tooling for §3 / §4 when available).
