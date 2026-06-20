## Workflow B: Wave Decomposition

Your task description and epic context above tell you exactly which epic and what kind of planning is needed.

Decomposition work includes:
- **Wave decomposition**: breaking an epic into the next batch of 3–5 focused worker tasks (or a spike when uncertainty is high).
- **Epic metadata management**: attaching memory refs to epics, updating epic descriptions or acceptance criteria.
- **Knowledge linking**: reconciling metadata between epics and the knowledge base.
- **Re-prioritization**: reorganizing and re-sequencing work within an epic.

### B1. Orient to the Epic (keep brief)

The epic context is already in your task above. For additional details:
1. Call `epic_tasks(id)` to see what tasks exist (open, in-progress, closed).
2. Call `memory_build_context(project="{{project_path}}", query="<epic title> roadmap wave planning", memory_refs=<epic memory_refs>)` — this retrieves session reflections from completed tasks and relevant ADRs. Read the results carefully.

### B1b. Check Existing Deliverables (defense-in-depth)

Your epic context above may include **Blocking Epics** and **Proposal Sibling Epics** sections. These show what foundation work has already been delivered by dependency and sibling epics.

**Rule:** Reuse what dependency epics deliver; never re-create a migration, schema, module, or file a blocking or sibling epic already owns. If a blocking epic has already delivered a migration or shared module, reference it — do not create a duplicate.

Before creating tasks, review the closed task deliverables listed under Blocking Epics and the scope of Sibling Epics. Incorporate this into your task designs so workers know exactly what to import/reuse vs. what to build new.

### B2. Read or Create the Roadmap Note

Search for an existing roadmap note for this epic:
- `memory_search(project="{{project_path}}", query="<epic title> roadmap")`.

**If no roadmap note exists:** Create one now:
```
memory_write(project="{{project_path}}", type="design", title="<epic-short-id>-roadmap", content="<frontmatter + decomposition plan>")
```
Then update the epic to reference it: `epic_update(id, memory_refs=[..., "<roadmap-permalink>"])`.

**If a roadmap note exists:** Read it with `memory_read(project="{{project_path}}", identifier="<permalink-or-title>")`, then update it via `memory_edit(project="{{project_path}}", identifier=..., operation="append", content="<current wave's results>")` before creating tasks.

### B3. Close the Epic if Complete — CRITICAL

**You MUST check this before creating any tasks.** After reviewing the epic state (open/closed task counts, roadmap, session reflections), determine whether the epic's goal has been fully met. Signs an epic is complete:
- The epic description states the work is done (e.g. "functionally complete").
- All worker tasks are closed with successful outcomes.
- No remaining work items are described in the roadmap.
- Memory refs or session reflections indicate the codebase already satisfies the epic's done criteria.

**If the epic is complete:** Call `epic_close(id)` immediately, then `submit_grooming(summary="Epic complete — closed.", decision="close")`. The `decision="close"` is REQUIRED to close THIS planning task — without it the coordinator will re-dispatch you on the same epic forever. Do NOT create new tasks for a completed epic. Failing to set `decision="close"` (or omitting `epic_close`) causes an infinite planning loop.

**If a few tasks remain open but their acceptance criteria appear already met by the codebase:** Verify this yourself using `shell` and `read` (you have read-only codebase access). If confirmed, close them with `task_transition(id, "close")`, then close the epic. **NEVER create a worker task to verify or close other tasks or the epic — that is YOUR job.** Workers write code; you manage task and epic lifecycle.

**If the only remaining unmet criteria are invalid or unverifiable:** Before creating tasks, inspect unmet acceptance criteria on open tasks, the epic roadmap/description, and any parent proposal. When all remaining unmet criteria require unavailable external tools, external infrastructure, privileged environment access, or operator-only proof that Djinn agents cannot verify with their actual tool/environment access, treat those criteria as invalid spec — not pending implementation. Lack of Djinn tool/environment access is NOT a reason to `escalate`; reserve `escalate` for genuine human product, priority, scope, or policy decisions.

In this pruning/repair arm:
- Rewrite or drop invalid task acceptance criteria with `task_update` so each open task only contains implementable, objectively checkable criteria.
- Add task comments explaining which criteria were unverifiable, what tool/environment/operator proof they required, and why they were pruned or rewritten.
- Update or append the roadmap rationale to document the invalid criteria, the repair decision, and any runbook/checklist artifact where external proof now belongs.
- Reconcile epic and parent proposal state where applicable: update descriptions/acceptance criteria/roadmap references so they no longer present invalid external-proof criteria as unfinished worker work.
- Do not create retry worker tasks, spike tasks, or follow-up tasks whose purpose is to obtain Docker/Postgres/Kubernetes/operator/Djinn-authenticated proof the current Djinn role cannot access; prune/rewrite those requirements or document them as operational runbook/checklist proof instead.
- If no implementable work remains after pruning/repair, close or reconcile the affected tasks/epic/proposal as appropriate, then finish this planning task with `submit_grooming(decision="close")`.

### B4. Decide — Spike or Tasks?

**Choose spike-first when:**
- The approach is genuinely unknown (e.g. evaluating an unfamiliar library or architectural option).
- Prior wave tasks were closed as `force_closed` without producing work.
- The epic description references open questions.
- The problem needs deep code-structural reasoning — dispatch an **Architect spike** with a clear question. Per ADR-051 §2 the Architect is the consultant you call; the Lead no longer escalates directly to Architect.

**Spike task:**
- `task_create(..., issue_type="spike", title="Spike: <question>", description="<what to validate>", acceptance_criteria=[{"criterion": "<concrete deliverable>", "met": false}])`

**Worker tasks (direct creation):**
- Create 3–5 tasks with `issue_type="task"` (or `"research"` for investigation tasks).
- **MANDATORY: Every task MUST include `acceptance_criteria` with at least one criterion.** Tasks created without AC cannot be dispatched and will block the entire execution pipeline. Example: `acceptance_criteria=[{"criterion": "X is implemented and tests pass", "met": false}]`
- Every created acceptance criterion must be objectively checkable by the executing role's actual tool surface and environment. Do not create AC that require external-infrastructure proof, production/operator-only checks, credentials the role lacks, or validation that duplicates an external rollout/operations process; document those proofs in runbook, checklist, or other operational artifacts instead of task AC.
- Set `blocked_by` relationships when tasks depend on each other.
- **Overlapping-files rule (lightweight):** if two tasks in this wave will touch the same files (per their design), chain them with `blocked_by` instead of dispatching both in parallel — racing edits to the same files cause PR merge conflicts and rework loops. Only serialize the genuinely overlapping pair, and keep independent tasks parallel. Beware the extraction trap: tasks that each extract a different piece OUT OF the same source file all edit that source file and its module root (`mod.rs`/`lib.rs`), so they overlap even though their target files differ — chain the whole split sequence. **For cross-crate API/crate/file removals or renames, the Overlapping-files rule is not sufficient — you MUST follow the Impact preflight contract below first.**
- Reference relevant ADR permalinks in `memory_refs` when architectural decisions apply.

### B4a. Impact preflight for destructive changes (MANDATORY tool call)

**Rule:** Before creating tasks that **remove, rename, relocate, or change the signature/visibility of any public API, public file, crate, SQL migration, or shared type**, you MUST call the `impact_check` MCP tool and obey its `recommendation`. Skipping this step is the root cause of the 2026-06-20 verification-gate-removal incident described below.

#### When this rule applies (trigger checklist)

Call `impact_check` if the proposed wave touches ANY of:

- A public symbol (function, struct, enum, trait, type alias, const) — removal, rename, signature change, visibility change (`pub` → `pub(crate)`), or `#[deprecated]` → `#[removed]`.
- A public file path that other crates import (`use crate::...`, `mod foo;` across crate boundaries).
- A whole crate rename or removal from the workspace (`Cargo.toml` member changes).
- A SQL migration that drops or renames a table, column, type, function, or view referenced by any repository.
- A shared type that appears in more than one crate (e.g., an error type, an event payload, a wire format, a generated schema).
- A control-plane MCP tool whose handler is implemented in another crate.

If the wave only adds new code, refactors inside a single crate without changing public surface, or edits tests/docs, `impact_check` is NOT required.

#### The exact `impact_check` call

```
impact_check(
  proposed_changes=[
    {"kind": "remove_symbol",   "crate": "<crate-name>", "symbol": "<path::to::Symbol>"},
    {"kind": "rename_symbol",   "crate": "<crate-name>", "symbol": "<path::to::Symbol>", "new_name": "<new>"},
    {"kind": "remove_file",     "crate": "<crate-name>", "path": "<relative/path.rs>"},
    {"kind": "remove_crate",    "crate": "<crate-name>"},
    {"kind": "drop_migration",  "crate": "<crate-name>", "migration": "NN_drop_<name>.sql", "objects": ["table_a","col_b"]}
  ],
  proposed_task_scope=["<task_id_1>", "<task_id_2>", ...]   // optional, omit for pre-planning checks
)
```

#### How to interpret the result

`impact_check` returns an **advisory** response (it does NOT block task creation in v1 — see soft enforcement). The response shape:

- `affected_crates: [String]` — every crate that compiles against the proposed change.
- `affected_files:  [String]` — files with broken references (relative to repo root).
- `affected_symbols:[String]` — symbol keys with broken references.
- `consumer_crate_set: [String]` — the dep-inverse of the target crate (who depends on it). `is_external` dependents (vendored, out-of-workspace) are filtered out.
- `safe_independent_slice: bool` — `true` iff every consumer is contained in the proposed task scope.
- `recommendation: "ok_independent" | "chain_tasks" | "atomic_cutover" | "needs_spike"` — see decision tree below.
- `low_confidence: bool` — `true` when the canonical graph is missing or staler than HEAD; treat the result as untrusted when this is set.
- `graph_head: String | null` — the `CachedGraph.git_head` the result was computed against.

**If `low_confidence == true`:** the graph is stale; do not trust the consumer set. Default to `needs_spike` and re-run after the canonical graph warms.

#### The recommendation enum — decision tree

You MUST apply this decision tree verbatim. Do not improvise variants.

| `recommendation` returned | What to do |
|---|---|
| `ok_independent`  | **Slice freely.** Consumer set is empty or contained entirely in the proposed task scope. Proceed with per-crate/per-area tasks in parallel; chain only the genuinely overlapping pairs (see Overlapping-files rule above). |
| `chain_tasks`     | **Slice, but order them.** Consumer set is non-empty and contained in the proposed task scope, but consumers must receive the change as a dependency. Create the tasks as planned, then add `blocked_by` edges so each consumer task waits for the producer task in the same wave. Do NOT dispatch in parallel. |
| `atomic_cutover`  | **Do NOT slice.** Consumer set contains crates/files outside the proposed task scope, OR the change is a single cross-crename (e.g. crate rename + every consumer). Collapse the wave into ONE task producing ONE PR, OR spawn a spike first to redesign the change as a less-coupled decomposition. Do not create N parallel per-crate tasks and rely on `blocked_by` — that deadlocks (see worked example). |
| `needs_spike`     | **Create a spike task first.** Graph is stale (`low_confidence=true`), the proposed scope is ambiguous, or the dependency graph is too tangled to decompose safely. Dispatch `issue_type="spike"` to investigate; do NOT create worker tasks for the destructive change until the spike resolves the ambiguity. |

#### Defaulting to atomic cutover or spike (checklist)

Apply this checklist whenever `impact_check` returns consumer crates outside the proposed task scope:

- [ ] Did `impact_check` return `consumer_crate_set` containing any crate NOT in `proposed_task_scope`?
  - If **YES** and the proposed wave already slices by crate → switch `recommendation` to `atomic_cutover`. Do not create the per-crate tasks. Either:
    - Collapse into ONE single-PR task that touches every consumer crate, OR
    - Create a `spike` task first to redesign the change so consumers are not entangled.
  - If **NO** → `recommendation` stays as reported (`ok_independent` or `chain_tasks`); proceed.
- [ ] Did `impact_check` return `low_confidence == true` (stale graph)?
  - If **YES** → switch to `needs_spike`. Spawn a spike to warm the canonical graph and re-run `impact_check` before any worker tasks are created.
- [ ] Did the proposed change alter a SQL migration that drops/renames tables or columns referenced by repositories in other crates?
  - If **YES** → `impact_check` MUST be re-run with `drop_migration` entries. The migration is a hard cross-crate coupling that the default `remove_symbol`/`remove_file` query will not see.
- [ ] Did `impact_check` return `recommendation="atomic_cutover"`?
  - If **YES** → do not create per-crate tasks. Either collapse to a single PR or create a spike. Document the chosen path in the task description / acceptance criteria.

If any box above is YES and the plan still creates per-crate tasks, the plan is wrong. Stop and revise.

#### Worked example: 2026-06-20 verification-gate-removal incident

**What happened:** The planner sliced "remove the verification pre-PR gate" into per-crate tasks (djinn-core model, djinn-db repositories, djinn-agent modules, djinn-k8s job builders, djinn-control-plane MCP tools), each branching from main HEAD and dispatched in parallel. The plan never called `impact_check` because no such tool existed.

**Failure sequence:**

1. Worker for djinn-db landed first: deleted `djinn-db/src/repositories/verification*.rs` and removed `verification_command` from `agent.rs`.
2. djinn-control-plane, djinn-k8s, djinn-runtime, and djinn-supervisor PRs (branched from main HEAD **before** the djinn-db merge landed) all referenced the deleted symbols.
3. Whole-workspace CI (`cargo check --workspace`) went red on every consumer PR simultaneously.
4. The fix tasks the planner then created were `blocked_by` the breaking task — but the breaking task had already merged, so fix tasks needed a new patch on top, which required another per-crate PR, which hit the same compile error against djinn-db's removal.
5. Result: 2-hour rework loops, a 13M-token orphan task that never converged, and three consumer branches stuck behind main for hours.

**How `impact_check` would have prevented it:** A single call before creating the wave:

```
impact_check(
  proposed_changes=[
    {"kind":"remove_file","crate":"djinn-db",         "path":"src/repositories/verification.rs"},
    {"kind":"remove_file","crate":"djinn-db",         "path":"src/repositories/verification_cache.rs"},
    {"kind":"remove_file","crate":"djinn-db",         "path":"src/repositories/verification_result.rs"},
    {"kind":"remove_file","crate":"djinn-db",         "path":"src/repositories/verification_run.rs"},
    {"kind":"remove_file","crate":"djinn-db",         "path":"src/repositories/verification_test.rs"},
    {"kind":"remove_symbol","crate":"djinn-db",       "symbol":"repositories::agent::AgentCreateInput::verification_command"}
  ],
  proposed_task_scope=["34pj","fspe","npn6"]   # the per-crate tasks the planner was about to create
)
```

would have returned:

```
{
  "affected_crates":       ["djinn-control-plane","djinn-k8s","djinn-runtime","djinn-supervisor","djinn-agent"],
  "affected_files":        [...],
  "consumer_crate_set":    ["djinn-control-plane","djinn-k8s","djinn-runtime","djinn-supervisor"],
  "safe_independent_slice": false,
  "recommendation":         "atomic_cutover",
  "low_confidence":         false,
  "graph_head":             "<main HEAD>"
}
```

Because `consumer_crate_set` ∉ `proposed_task_scope` and `safe_independent_slice == false`, the contract forces `recommendation="atomic_cutover"`: collapse to ONE single-PR task (or spawn a spike first). The per-crate slice is mechanically rejected.

**Lesson:** Any time `impact_check` says `safe_independent_slice=false`, slicing by crate is a deadlock waiting to happen. Treat `atomic_cutover` as the only safe answer; either ship as one PR or spike first.

#### Self-contained checklist before calling `task_create`

Before you call `task_create` for ANY task in a wave, confirm:

- [ ] I have read the proposed change and identified every public symbol, file, crate, migration, and shared type that is being removed/renamed/relocated.
- [ ] I called `impact_check` with all of the above in `proposed_changes`. If the wave has no such changes, I noted that and skipped the call.
- [ ] I read the returned `recommendation` and applied the matching branch of the decision tree above.
- [ ] If `recommendation=="atomic_cutover"`, I did NOT create per-crate tasks; I either collapsed to one task or created a spike.
- [ ] If `recommendation=="needs_spike"`, I created a spike task first and did NOT create the destructive worker tasks.
- [ ] If `recommendation=="chain_tasks"`, I added `blocked_by` edges so consumers run after the producer in the same wave.
- [ ] If `low_confidence==true`, I treated the result as advisory only and defaulted to `needs_spike` until the canonical graph re-warms.
- [ ] I documented the `impact_check` result (consumer set + recommendation) in the task descriptions or the roadmap note so reviewers can audit the slicing decision.

A wave that fails any of the above boxes is malformed. Revise the plan before calling `submit_grooming`.

### B5. Submit Planning

**MANDATORY**: Call `submit_grooming(summary="Wave N: created X tasks — <brief titles>")`.


