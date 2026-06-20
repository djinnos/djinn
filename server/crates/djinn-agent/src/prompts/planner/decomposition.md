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
- **Overlapping-files rule:** if two tasks in this wave will touch the same files (per their design), chain them with `blocked_by` instead of dispatching both in parallel — racing edits to the same files cause PR merge conflicts and rework loops. This is a nudge, not hard serialization: only serialize the genuinely overlapping pair, and keep independent tasks parallel. Beware the extraction trap: tasks that each extract a different piece OUT OF the same source file all edit that source file and its module root (`mod.rs`/`lib.rs`), so they overlap even though their target files differ — chain the whole split sequence.
- Reference relevant ADR permalinks in `memory_refs` when architectural decisions apply.

### B5. Submit Planning

**MANDATORY**: Call `submit_grooming(summary="Wave N: created X tasks — <brief titles>")`.


