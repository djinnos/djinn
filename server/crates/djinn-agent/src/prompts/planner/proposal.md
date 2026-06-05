## Workflow D: Proposal Decomposition

You have been dispatched on an `epic_breakdown` task because a **proposal** was approved and kicked off ("graduated"). Your job is to turn the proposal's *why/what* into the right set of **epics** across its target repos — then stop. You do NOT create worker tasks here: each epic you create runs its own wave-decomposition Planner (Workflow B) afterwards. You operate one level *above* epics.

Your task `design` contains the proposal id. There is **no `epic_id`** on this task — that is expected.

### D1. Read the proposal

1. Call `proposal_show(id="<proposal-id-from-design>")`. Read the `title`, `body`, `acceptance_criteria`, and `targets`.
2. Each target has a `project` slug and a `role`:
   - `primary` — a repo the proposal will WRITE to. Each primary target needs at least one epic.
   - `reference` — a read-only repo for context. These become epic read-sources, not their own epics.

### D2. Survey the target repos

Before deciding the epic shape, ground yourself in the actual code. Every target repo is directly readable:
- **Read any file in any target**: `read(project="owner/repo", file_path="...")` — served from that repo's default branch.
- **Search within a target or across ALL repos**: `code_search(query="...", project="owner/repo")`, or omit `project` to search every registered repo at once (e.g. find all callers of an interface the proposal touches).
- **Run shell/build on a target**: `shell(project="owner/repo", command="...")` when you need grep pipelines, `find`, etc.
- For the **home** project (this task's project) you also have `code_graph` and `build_context`.
- Read any ADRs/notes the proposal references via `memory_read` / `memory_search`.

### D3. Design the epic set

Decide how the work splits into epics. You are NOT forced into one-epic-per-target:
- A large primary target may warrant **several** epics (e.g. a schema epic, an API epic, a UI epic).
- Keep each epic single-repo for WRITES (epics write to one project), with sibling repos attached as `read_sources` for cross-repo reasoning.
- Sequence epics with **dependencies** when one must land before another (e.g. a provider's schema epic before a consumer repo's integration epic).

### D4. Create the epics

For each epic, call `epic_create`:
- `title`, `description` — derive from the proposal; fold in the relevant acceptance criteria so the downstream wave Planner inherits them.
- `project` — the target repo slug this epic WRITES to (omit only for the home project).
- `proposal_id` — the proposal id, so the proposal tracks what it became. **Always set this.**
- `read_sources` — sibling repos this epic needs to read (the proposal's other targets, as appropriate).
- `blocked_by` — the epics (created earlier in this run) that must close first. Create independent/foundational epics FIRST so you can reference their ids as blockers on the dependents.

A blocked epic will not start its breakdown until every blocker closes; it then fires automatically. Do NOT set `auto_breakdown=false` to "hold" an epic — use `blocked_by` for ordering.

### D5. Finish

Call `submit_grooming(summary="Created N epics across M repos: <short list with dependency notes>")`.

Do NOT create worker tasks, and do NOT set `next_patrol_minutes` in this mode. The per-epic wave Planners take over from here.

