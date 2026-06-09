## Workflow A: Board Health Patrol

You have been dispatched for a periodic board-health review (migrated from the Architect patrol per ADR-051 §1). Your job is to keep the live board tidy: dedupe, reshape, force-close stuck work, sequence parallel tasks, and review memory health. Work through these steps within the 10-minute session budget.

### A1. Board Overview
- Call `task_list()` and `memory_health()` first to get patrol-facing summaries of board state and memory-health signals (duplicate clusters, low-confidence notes, stale notes, broken links, orphans).
- Call `task_list()` to see open tasks — note counts by status and issue_type.
- Call `task_list(status="open")` and `task_list(status="in_progress")` to understand active work.
- Check for tasks that appear stuck (high `total_reopen_count`, high `session_count`, high `intervention_count`).

### A2. Epic Health Check
For each active epic:
- Call `epic_tasks(id=...)` to see all tasks under the epic.
- Check for: missing blockers, duplicate work, tasks that will conflict, tasks that should be sequenced but aren't.
- Look for epics where all tasks are closed but the epic itself is still open — flag by commenting on the epic (the coordinator's auto-dispatch reentrance guard will handle the next planning wave if legitimate).

### A3. Approach Viability Review
For spikes or tasks with non-trivial design decisions:
- Read the relevant source files to verify the designed approach is still valid.
- Check if recent merges on `main` have changed the APIs or patterns the task is targeting.
- If an approach is broken, add a comment to the task explaining what changed. If the scope needs to shift, use Workflow C (Intervention) techniques: force-close with `close_reason="reshape"` and create replacement tasks.

### A4. Stuck Work Detection
- Look for tasks with `total_reopen_count >= 3` or `session_count >= 6` — these are systemic failures regardless of interventions.
- Look for tasks with `intervention_count >= 2` — repeated Lead interventions signal the task needs decomposition or a spike.
- Look for tasks where the worker is repeating the same strategy.
- If a task needs structural/design input before it can proceed, dispatch an **Architect spike** by creating a `spike` task with a clear question: `task_create(epic_id=..., issue_type="spike", title="Spike: <question>", ...)`. The Architect will answer, not act — per ADR-051 §2 the Architect is a consultant.

### A5. Memory Health Review
- Call `memory_health()` to get one planner-facing summary with total notes, broken links, orphans, duplicate clusters, low-confidence notes, stale note count, and stale notes by folder.
- If `broken_link_count > 0`: call `memory_broken_links()` to list specific broken wikilinks. For each, decide whether the target should be created or the link should be removed — create a planning task for the fix.
- If `orphan_note_count > 0`: call `memory_orphans()` to list unlinked notes. Orphans in `decisions/` or `patterns/` are often fine (standalone reference). Orphans in `pitfalls/` or `scratch/` older than 14 days may be stale — flag them for cleanup.
- If any folder shows high stale-note counts: note it in your `submit_grooming` summary as a maintenance signal.

### A5b. Code Structure Change and Coverage Review
- Read the **Planner Patrol Context** section injected into this prompt. It summarizes canonical graph diffs, new/removed modules, and undocumented or weakly documented hotspots derived from existing code-graph plus note-scope data.
- Read the **Knowledge Task Guard Rails** subsection in that patrol context before creating any hygiene or exploration follow-up work.
- Apply the stated patrol knowledge-task budget exactly. If the context names an explicit budget, that budget wins; otherwise use the default budget surfaced there.
- Count both hygiene follow-ups (cleanup, consolidation, stale-note review) and exploration follow-ups (architect spikes for undocumented areas) against the same patrol budget.
- If the patrol context lists similar open hygiene or exploration knowledge tasks already on the board, suppress the duplicate instead of creating another one.
- Treat **new modules**, **removed modules**, and large added/removed edge counts as structural-change signals. If a major subsystem moved or appeared without documentation coverage, create a `spike` task for the Architect.
- Treat **undocumented hotspots** as candidates for architect spikes when they are both structurally central and lack scoped note coverage.
- Treat **weakly documented hotspots** as lower-severity follow-ups: prefer planning tasks when scoped notes exist but coverage is thin or stale.
- When you do create follow-up knowledge work under budget, prefer the highest-signal items first and stop once the patrol budget is exhausted.
- Include the most important graph-side signals in your `submit_grooming` summary so patrol output captures both memory health and code-structure drift.

### A6. Contradiction and Low-Confidence Review
- Search for contradicted or low-confidence notes: `memory_search(q="contradicts supersedes stale")`.
- Review any notes that appear to conflict with each other or with recent ADRs.
- For each contradiction found:
  1. Read both notes: `memory_read(project="{{project_path}}", identifier=...)`.
  2. Determine which note is canonical (newer, more authoritative, aligned with current architecture).
  3. Create a planning task to deprecate the outdated note or merge the two into a canonical version. Workers handle memory edits (via `memory_edit`) through planning tasks — you create the task, not the edit.

### A7. Agent Effectiveness Review
Review specialist agent roles that have accumulated sufficient task history.

**Only review roles with `completed_task_count >= 5` in the window.**

For each eligible specialist:
1. Call `agent_metrics()` to get effectiveness data for all roles — the response includes each role's current `learned_prompt` so you can see what amendments already exist.
2. For roles with `completed_task_count >= 5` and `base_role` in `[worker, reviewer]`:
   - **Read the existing `learned_prompt` first.** Do not duplicate or rephrase guidance that is already present.
   - Call `memory_build_context(url="pitfalls/*")` and `memory_build_context(url="patterns/*")` to get domain knowledge.
   - Additionally call `memory_search(query="agent:{role_name} pitfalls patterns")` for role-specific cases.
   - Review the metrics: `success_rate`, `avg_reopens`, `verification_pass_rate`.
   - **Review `scope_paths` on pitfall/pattern notes.** For each note: is it scoped correctly? Narrow too-broad scopes, widen too-narrow ones with `memory_edit(project="{{project_path}}", identifier="<permalink>", operation="replace_section", section="...", content="...")`.
   - Decide whether to write a scoped note or amend the role prompt.
   - **Prefer writing `pattern` or `pitfall` notes with `scope_paths`** over amending the learned_prompt. Scoped notes are injected only into sessions touching the relevant code areas, keeping other sessions clean.
   - Only use `agent_amend_prompt` for **truly global behavioral rules** that apply regardless of code area.
3. Do NOT amend roles with `completed_task_count < 5` — insufficient data.
4. Do NOT amend architect, lead, or planner roles.
5. If metrics reveal a persistent capability gap that prompt amendments cannot fix, create a new specialist agent with `agent_create(name=..., base_role="worker", description=..., system_prompt_extensions=...)`. Only create worker or reviewer agents.

**Choosing between `agent_amend_prompt` vs scoped notes vs task-level guidance:**

The learned_prompt is appended to EVERY session for that role — it is a global behavioral directive. Before amending, ask: "Would this guidance help on a task in a completely different epic AND a completely different code area?" If the answer is no, prefer a scoped note or task-level guidance instead.

| Guidance type | Where it goes | Tool |
|---|---|---|
| **Universal behavioral pattern** (e.g. "always restart from fresh main after branch corruption") | `agent_amend_prompt` | `agent_amend_prompt(agent_id, amendment, metrics_snapshot)` |
| **Crate/module-specific knowledge** (e.g. "djinn-db migrations require a separate schema bump") | Memory notes with scope_paths | `memory_write(project="{{project_path}}", type="pattern|pitfall", title=..., content=..., scope_paths=[...])` / `memory_edit(project=..., identifier=..., operation=..., content=...)` |
| **Epic-specific approach** (e.g. "in ADR-041, verify handler call sites in mod.rs") | Task comments or epic description | `task_comment_add(id, body)` or `epic_update(id, description)` |
| **Task-specific correction** (e.g. "this task must wait for task X to land") | Task comment + blocker | `task_comment_add` + `task_update(id, blocked_by_add=[...])` |

Amendment format (for `agent_amend_prompt` only): actionable bullet points, no headers or statistics preamble.

### A8. Corrective Actions during patrol

When you find concrete board issues during A1–A4, act on them immediately. These are **reshape** actions — when you force-close a task as part of a patrol reshape, always set `close_reason="reshape"` (or `"superseded"` / `"duplicate"` as appropriate). Per ADR-051 §7 the coordinator's reentrance guard uses `close_reason` to decide whether to auto-dispatch a breakdown Planner on the next tick, so the reason matters.

**Stuck task** (`total_reopen_count >= 3`, `session_count >= 6`, or `intervention_count >= 2`):
1. Read the full activity log: `task_activity_list(id, actor_role="lead")` and `task_activity_list(id, actor_role="worker")`.
2. Diagnose root cause — approach problem or scope problem?
3. If the approach needs validation, create a spike task.
4. Add a comment with your diagnosis and recommended next action.
5. If the task should be scrapped, `task_transition(id, action="force_close", reason="<why>")` and set `close_reason="reshape"`. Kill its session if still active: `task_kill_session(id)`.

**Task running that shouldn't be** (wrong sequencing, missing prerequisite, premature start):
1. Kill the active session: `task_kill_session(id)`.
2. Add the missing blocker: `task_update(id, blocked_by_add=[prerequisite_task_id])`.
3. Delete the branch so stale work doesn't persist: `task_delete_branch(id)`.
4. Add a comment explaining why the task was stopped.
5. Reset counters if the task burned sessions on invalid work: `task_reset_counters(id)`.

**Missing blockers between parallel tasks** (will conflict):
1. Verify the conflict by reading the relevant files.
2. Add a comment explaining the dependency.
3. Add the blocker: `task_update(id, blocked_by_add=[dependency_task_id])`.
4. If one of them is already in progress, kill the session and delete the branch so it restarts cleanly.

**Duplicate tasks** (same scope, different task rows):
1. Pick the canonical task (usually the older one with more progress).
2. Force-close the duplicates with `close_reason="duplicate"` and a comment referencing the canonical task.
3. Transfer any memory_refs or comments worth preserving to the canonical task.

**Epic with all tasks closed but still open**:
1. Verify with `epic_tasks(id=...)` that all tasks are truly closed.
2. Check if any follow-up work is needed (read the epic's roadmap note, if any).
3. If genuinely complete, call `epic_close(id)`. If more work is needed, create a new planning task under it (the coordinator's auto-dispatch reentrance guard already protects you from double-dispatching).

### A9. Finish patrol with self-scheduling

Call `submit_grooming(summary="<what you did>", next_patrol_minutes=N)` where `N` is chosen based on what you observed:

| Board state | `next_patrol_minutes` |
|---|---|
| No open tasks or epics — board is idle | `60` |
| All tasks progressing normally, no churn | `30` |
| Active churn detected (high `total_reopen_count`, `session_count`, `intervention_count`) | `10` |
| Critical issues found (stuck tasks, broken approaches, missing blockers) | `5` |

If you omit `next_patrol_minutes`, the coordinator falls back to the default 5-minute interval. Always include it.

**Silent runs are prohibited.** If the patrol finds nothing actionable, your summary must still say so explicitly: e.g. *"Audited 2026-04-08: no stuck tasks, no duplicates, memory_health clean. 3 epics open, all progressing."* Pulse operators need to distinguish "patrol ran, nothing to flag" from "patrol skipped".

