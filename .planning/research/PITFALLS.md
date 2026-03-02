# Pitfalls Research

**Domain:** AI agent planning system — fork of GSD adapted for MCP-based memory and task storage
**Researched:** 2026-03-02
**Confidence:** HIGH (codebase analysis) / MEDIUM (MCP integration patterns from industry sources)

## Critical Pitfalls

### Pitfall 1: Ghost State — Duplicating GSD's Filesystem State Machine in MCP

**What goes wrong:**
GSD has a deeply coupled state machine in `state.cjs` (680 lines) that reads and writes `STATE.md` as the source of truth — tracking current phase, current plan, status, progress percentages, session info, decisions, blockers, and performance metrics. This state file has YAML frontmatter synced on every write, field-level patching, and a progression engine (`stateAdvancePlan`, `stateUpdateProgress`). The temptation is to recreate this state machine using Djinn memory notes, effectively building a second state tracker that shadows Djinn's own task system.

**Why it happens:**
GSD workflows constantly call `gsd-tools.cjs state load`, `state snapshot`, `state json` to decide what to do next. When porting workflows, it feels necessary to preserve this same "where am I?" system. But Djinn's task board already tracks status (`open`, `in_progress`, `needs_task_review`, etc.), and its execution engine tracks phase state (`waiting`, `active`, `reviewing`, `completed`). Building a parallel state system creates divergence — the memory note says "Phase 2, Plan 3" while the task board says something different.

**How to avoid:**
Derive progress state from Djinn's task and execution APIs, never from a stored state document. Replace `state load` calls with `task_list(status="in_progress")` + `execution_phase_list()`. If a `/djinn:progress` command needs to report status, it queries the live task/execution state, not a cached note. The only state that should persist in memory is the project brief and roadmap — these are reference documents, not live state.

**Warning signs:**
- A memory note that tracks "Current Phase" or "Current Plan"
- Any workflow that writes to a state note and reads from it later in the same session
- `memory_edit` calls that update progress fields
- A "state sync" step that reconciles memory notes with task board state

**Phase to address:**
Phase 1 (Core Workflow Adaptation) — must be a design constraint from day one, not a refactor.

---

### Pitfall 2: The Impedance Mismatch — GSD's File-Based Plan Identity vs. Djinn's UUID Task Identity

**What goes wrong:**
GSD identifies plans by filesystem path: `.planning/phases/01-setup/01-01-PLAN.md`. This path encodes phase number (01), plan number (01-01), and the phase name (setup). Plan ordering is by filename. Wave grouping is frontmatter in the plan file. Summaries are matched by naming convention (`01-01-SUMMARY.md`). When porting to Djinn, plans become tasks identified by UUIDs or short IDs (`k7m2`). There is no inherent ordering, no naming convention for completion tracking, and wave grouping must be expressed as blocker dependencies. If you naively port the workflows but keep referencing plans by their old naming scheme, you lose the mapping between GSD plan identities and Djinn task IDs.

**Why it happens:**
GSD's `phase.cjs` has 44 filesystem calls — `readdirSync`, `readFileSync`, etc. — to enumerate plans, check summaries, build indexes. The entire plan lifecycle is path-based. Porting to MCP means replacing path-based identity with ID-based identity, but the workflow templates still think in terms of "Phase 1, Plan 3."

**How to avoid:**
Use Djinn's hierarchy directly: milestones map to epics, phases to features (child of epic), plans to tasks (child of feature). Use labels (`gsd:phase-1`, `gsd:plan-1-3`, `gsd:wave-2`) for GSD-native querying but always resolve to Djinn IDs for operations. The cookbook's import pattern (Steps 1-5 in `gsd.md`) already demonstrates this, but the adapted workflows must never store or rely on filesystem paths — only Djinn IDs and labels.

**Warning signs:**
- Workflow templates containing `.planning/phases/` paths
- Code that constructs plan filenames or parses phase directories
- Any import step that creates a mapping file between GSD paths and Djinn IDs (this mapping should be transient, not persisted)
- Labels being used as primary identifiers instead of task IDs

**Phase to address:**
Phase 1 (Core Workflow Adaptation) — the identity mapping is foundational. Get it wrong and every subsequent workflow breaks.

---

### Pitfall 3: MCP Tool Explosion — Overloading Agent Context with Tool Descriptions

**What goes wrong:**
Djinn exposes 70+ MCP tools (task CRUD, memory CRUD, execution management, settings, projects, sync). Every tool's JSON schema must fit in the agent's context window. GSD workflows already consume significant context with their templates (the `new-project.md` workflow alone is 500+ lines). When ported workflows also need 70+ tool schemas loaded, the agent's effective reasoning space shrinks dramatically. Industry research confirms: "By the time the LLM is taught how to use a particular API you've gobbled up a bunch of your context window."

**Why it happens:**
MCP tool registration is all-or-nothing per server — you get every tool the Djinn server exposes. GSD solved this by using a CLI (`gsd-tools.cjs`) with subcommands that the agent already knew from training data. MCP tools are unfamiliar and require schema understanding per invocation.

**How to avoid:**
The skill/cookbook layering already in `SKILL.md` is the right pattern — the top-level skill is a routing document that loads specific cookbooks on demand. Extend this: each workflow (`new-project`, `plan-phase`, `discuss-phase`) should declare which Djinn MCP tools it actually uses (typically 5-10 out of 70+). Include only those tool patterns in the workflow context. The SKILL.md's "Common Mistakes" table is a good precedent — small, focused guidance beats exhaustive documentation.

**Warning signs:**
- Workflow templates that include full MCP tool documentation
- Agents making incorrect tool calls (wrong tool name, wrong parameters) — indicates context overload
- Agent responses that discuss Djinn tools instead of doing work — the "teaching itself" anti-pattern
- Workflows that reference tools they never use

**Phase to address:**
Phase 1 (Core Workflow Adaptation) — each workflow template must declare its tool subset.

---

### Pitfall 4: Losing the Methodology While Porting the Mechanism

**What goes wrong:**
GSD's value is not its file management — it is the *methodology*: deep questioning that follows threads (not checklists), parallel research across 4 dimensions, requirement IDs with traceability, wave-based plan ordering, revision loops with plan checkers. The risk is getting so focused on "replace `fs.readFileSync` with `memory_read`" that the methodology gets diluted. The `new-project.md` workflow's questioning phase is a careful conversational flow — if porting flattens it into "collect answers, write to memory," the quality of project setup degrades.

**Why it happens:**
Filesystem operations are concrete and easy to port ("replace this file write with that MCP call"). Methodology is embedded in prompt structure, agent instructions, and multi-agent orchestration patterns that are harder to preserve during adaptation. Developers focus on what's measurable (tool calls working) over what's qualitative (questioning depth).

**How to avoid:**
Port methodology first, mechanism second. For each workflow: (1) extract the methodology (what decisions are being made, what quality is being enforced, what patterns are being followed), (2) document it as a specification independent of storage, (3) then implement the storage adapter. Concretely: the `new-project` workflow's questioning methodology should be ported verbatim from GSD — only the output destinations change (memory_write instead of file write).

**Warning signs:**
- Workflow templates significantly shorter than GSD originals (methodology was cut, not just file I/O)
- Missing revision loops (GSD's plan-checker loop is 3 iterations — if the port has none, methodology was lost)
- Questioning phase reduced to a form or checklist instead of conversational flow
- Research agents output less structured content than GSD's dimension-specific templates

**Phase to address:**
Phase 1 (Core Workflow Adaptation) — methodology preservation is the acceptance criteria for every ported workflow, not "does the MCP call work."

---

### Pitfall 5: MCP Server Dependency Without Graceful Degradation

**What goes wrong:**
PROJECT.md says "MCP Required: All runtimes need djinn-server running — no filesystem fallback." This is a deliberate constraint, but it means every workflow crashes hard if the MCP server is unavailable, slow, or returns errors. GSD's filesystem operations are local and near-instant. MCP calls go through a transport layer (stdio bridge, HTTP) that can fail. A flaky MCP connection mid-workflow means lost work — an agent halfway through creating tasks gets a connection error and the partial state is inconsistent (some tasks created, some not, blockers half-set).

**Why it happens:**
MCP is treated as if it has filesystem reliability. But it is a client-server protocol with serialization, transport, and server-side processing. The Nearform MCP guide warns: "STDIO logging contamination" can corrupt the transport, and "connection handling" for long-lived streams is fragile with reverse proxies.

**How to avoid:**
Design workflows to be idempotent and resumable. Every task creation batch should be atomic (use `execution_apply_changes` for batch mutations, not sequential `task_create` calls). Every workflow should have a "where was I?" check that queries existing state before continuing. For example, `new-project` should check `memory_search(query="brief")` before creating a new brief — if one exists, resume from where it left off. The `task_create` calls during plan import should be wrapped in a pattern that checks for existing tasks by title/label before creating duplicates.

**Warning signs:**
- Workflows that create 10+ tasks in sequential calls without checking for partial completion
- No idempotency checks in any workflow
- Agent prompts that assume "start from scratch" without checking existing state
- Error handling that says "retry the entire workflow" instead of "resume from last successful step"

**Phase to address:**
Phase 1 (Core Workflow Adaptation) — every workflow must include a "resume check" as its first step. This parallels GSD's `init` command pattern which loads existing state before proceeding.

---

### Pitfall 6: The Hierarchy Mapping Trap — Milestones as Epics vs. Phases as Epics

**What goes wrong:**
PROJECT.md lists two contradictory mappings:

1. The hierarchy table says: Milestone -> Epic, Phase -> Feature, Plan/Task -> Task
2. The "Key Decisions" section says: "Phases -> Epics not Features: Epics give better visibility in kanban"

This ambiguity will infect every workflow. If phases are epics, then milestone is... what? A label? A project? If milestones are epics and phases are features, that matches Djinn's hierarchy but gives less kanban visibility. The cookbook `gsd.md` adds a third variant: it creates an epic for the milestone and features for phases. Getting this wrong means the entire task hierarchy is inconsistent, queries return wrong results, and the execution engine groups tasks incorrectly.

**Why it happens:**
GSD has a 3-level hierarchy (milestone > phase > plan) but Djinn has a 4-level one (epic > feature > task > bug). The extra level creates mapping ambiguity. Both mappings are plausible, and different parts of the documentation chose differently.

**How to avoid:**
Resolve this in an ADR before writing any workflow code. The cookbook `gsd.md` pattern (milestone = epic, phase = feature, plan = task) is the correct mapping because: (a) it preserves the full hierarchy without collapsing levels, (b) features already represent 2-4 hour deliverables which matches GSD phase scope, (c) the execution engine already groups by feature parent for phase planning. Write this as an ADR in Djinn memory, reference it from every workflow, and remove the contradictory statement from PROJECT.md.

**Warning signs:**
- Different workflows using different hierarchy mappings
- Epics that contain tasks directly (skipping feature level)
- `task_create` calls with `issue_type="epic"` for phases in some workflows and `issue_type="feature"` in others
- Queries that return unexpected results because the hierarchy is inconsistent

**Phase to address:**
Phase 0 (Architecture Decisions) — this must be locked before any workflow adaptation begins. One ADR, one mapping, enforced everywhere.

---

### Pitfall 7: Workflow Template Prompt Size Explosion

**What goes wrong:**
GSD's workflow templates are already large (500-1000 lines each). They reference other files via `@` includes and delegate to subagents who load fresh context. When porting to Djinn, the temptation is to inline MCP tool documentation, Djinn cookbook patterns, and memory type schemas into each workflow template. This creates workflow files that are 2000+ lines and blow past effective prompt budgets. The agent either truncates critical instructions or loses coherence.

**Why it happens:**
GSD workflows assume the agent knows filesystem operations (bash, cat, ls) from pre-training. MCP operations are not in pre-training data, so they need explicit documentation. Each workflow ends up duplicating Djinn usage patterns.

**How to avoid:**
Follow GSD's own architecture: orchestrator workflows stay lean (coordinate, don't execute), subagents load their own context. The Djinn skill's cookbook system is the right pattern — reference cookbooks by path, don't inline them. Each workflow should be: (1) a methodology document (ported from GSD), (2) a short "MCP adapter" section that maps GSD outputs to Djinn tool calls, (3) references to cookbooks for tool details. Target: no workflow template exceeds 600 lines including MCP adapter sections.

**Warning signs:**
- Workflow templates over 800 lines
- Same MCP tool call patterns duplicated across multiple workflows
- Agents that "lose the plot" mid-workflow (context exhaustion)
- Instructions near the bottom of long templates being ignored

**Phase to address:**
Phase 1 (Core Workflow Adaptation) — enforce a line budget per workflow template.

## Technical Debt Patterns

| Shortcut | Immediate Benefit | Long-term Cost | When Acceptable |
|----------|-------------------|----------------|-----------------|
| Hardcode project path in workflows | Quick testing, no path resolution logic | Breaks multi-project support, non-portable | Never — use `project` parameter from context |
| Skip `memory_search` before `memory_write` | Faster workflow execution | Duplicate notes accumulate, search results degrade | Never — always search first per SKILL.md guidance |
| Use description field for acceptance criteria | Works in a single agent session | Review pipeline cannot verify criteria, agents cannot self-check | Never — Djinn has a dedicated `acceptance_criteria` array field |
| Sequential task creation without batch operations | Simpler workflow logic | Partial failure leaves inconsistent state, slower execution | Only for single task creation; batches should use `execution_apply_changes` |
| Store GSD plan content verbatim in task description | Preserves all plan detail | Task descriptions become 500+ lines, agents cannot parse them effectively | Only for initial import — distill into description + design + AC fields |

## Integration Gotchas

| Integration | Common Mistake | Correct Approach |
|-------------|----------------|------------------|
| Djinn memory singletons (brief, roadmap) | Calling `memory_write` with `type="brief"` when one already exists — overwrites without warning | Always `memory_read(identifier="brief")` first; use `memory_edit` to update existing |
| Djinn task blockers | Setting blockers on epics/features instead of tasks | Epics cannot participate in blocker relationships — only task, feature, and bug types. Wave ordering must block task-to-task |
| Djinn execution phases | Creating phases manually when `execution_start` auto-groups | Use `execution_preview_unified` first to see what auto-grouping produces. Only use `execution_launch_explicit` when auto-grouping is wrong |
| Multi-runtime distribution | Assuming all runtimes load skills the same way | Claude Code uses plugin/skills; OpenCode uses MCP config; Gemini uses extensions. The installer pattern from GSD must be adapted per runtime |
| Djinn memory wikilinks | Writing `[[Note Title]]` without verifying the target note exists | Use `memory_search` to find exact titles. Broken wikilinks are common — run `memory_broken_links()` periodically |
| MCP stdio transport | Logging debug output to stdout in the bridge process | Any stdout contamination corrupts the MCP JSON-RPC stream. Use stderr for logging |

## Performance Traps

| Trap | Symptoms | Prevention | When It Breaks |
|------|----------|------------|----------------|
| Sequential MCP calls for batch operations | Workflow takes 30+ seconds to create 10 tasks with blockers | Use `execution_apply_changes` for atomic batch mutations | 5+ tasks with inter-dependencies |
| `memory_catalog()` on every session start with large KB | Agent stalls for seconds loading catalog at session start | Cache catalog results; use `memory_search` for targeted lookups | 100+ notes in knowledge base |
| Loading full task descriptions during `task_list` | Token budget consumed by task listing, not by reasoning | Use `detail_level="brief"` for listing; `task_show` for full details | 20+ tasks on the board |
| Unbounded `memory_build_context` depth | Traverses entire knowledge graph, returns massive response | Always set `depth=1` or `depth=2` with `max_related=10` | 50+ interconnected notes |

## "Looks Done But Isn't" Checklist

- [ ] **Workflow porting:** Methodology preserved — compare questioning depth/revision loops against GSD original, not just "does it write to memory"
- [ ] **Task creation:** All tasks have `parent` set — orphaned tasks invisible to execution engine queries
- [ ] **Wave ordering:** Blocker dependencies actually set between wave N+1 tasks and wave N tasks — not just labeled
- [ ] **Memory notes:** Wikilinks resolve to actual notes — run `memory_broken_links()` after creation
- [ ] **Multi-runtime support:** Installer tested on at least Claude Code + one other runtime (OpenCode) — not just the development environment
- [ ] **Idempotency:** Running a workflow twice does not create duplicate tasks/notes — search-before-create pattern verified
- [ ] **Hierarchy consistency:** Every task_create uses the same mapping (milestone=epic, phase=feature, plan=task) — spot-check by querying `task_children_list` on each epic
- [ ] **Acceptance criteria:** AC is in `acceptance_criteria` field, not in `description` — verify with `task_show` on sample tasks
- [ ] **Research output:** All 4 dimension agents produce output stored in Djinn memory, not in `.planning/` files

## Recovery Strategies

| Pitfall | Recovery Cost | Recovery Steps |
|---------|---------------|----------------|
| Ghost state (duplicate state tracking) | MEDIUM | Delete state memory notes. Audit workflows to remove state writes. Replace with live task queries |
| Hierarchy mismatch (inconsistent mapping) | HIGH | Requires re-creating all tasks under correct parent types. Export current tasks, delete, re-import with correct hierarchy |
| Methodology loss | HIGH | Compare each ported workflow against GSD original line-by-line. Restore questioning/revision sections. No shortcut — manual review required |
| MCP partial failure (inconsistent task state) | LOW | Query existing tasks by label, delete partial duplicates, re-run import with idempotency checks |
| Prompt explosion (oversized workflows) | MEDIUM | Extract MCP patterns into shared cookbooks. Reduce each workflow to methodology + adapter. Measurable: line count per file |
| Broken wikilinks after memory migration | LOW | Run `memory_broken_links()`. Fix or remove broken links. Automated — one-time cleanup |

## Pitfall-to-Phase Mapping

| Pitfall | Prevention Phase | Verification |
|---------|------------------|--------------|
| Ghost state (P1) | Phase 0: ADR "no stored state — derive from task/execution APIs" | Grep all workflows for `memory_write`/`memory_edit` calls that update progress/status fields. Should find zero |
| Identity mismatch (P2) | Phase 1: Workflow adaptation | Grep for `.planning/phases/` in all workflow templates. Should find zero references to filesystem paths |
| MCP tool explosion (P3) | Phase 1: Workflow adaptation | Each workflow template declares its tool subset in a comment block. Count: max 10 tools per workflow |
| Methodology loss (P4) | Phase 1: Workflow adaptation | Diff each ported workflow against GSD original. Every methodology section must have a corresponding section. Revision loops preserved |
| MCP server dependency (P5) | Phase 1: Workflow adaptation | Every workflow starts with a "resume check" that queries existing state. Test: kill server mid-workflow, restart, re-run — no duplicates created |
| Hierarchy mapping (P6) | Phase 0: ADR "hierarchy mapping" | Single ADR in Djinn memory. PROJECT.md updated to remove contradiction. All task_create calls use consistent mapping |
| Prompt size explosion (P7) | Phase 1: Workflow adaptation | `wc -l` on each workflow template. Max 600 lines. MCP patterns extracted to cookbooks |

## Sources

- GSD source analysis: `state.cjs` (680 lines, 32 filesystem calls), `phase.cjs` (44 filesystem calls), `config.cjs` (12 filesystem calls), 10 CJS modules with 212 total fs calls, 152 references to `.planning/`
- Djinn codebase: `SKILL.md`, cookbooks (`gsd.md`, `work-decomposition.md`, `execution-planning.md`, `memory-management.md`), `PROJECT.md`
- [MCP: Model Context Pitfalls in an Agentic World — HiddenLayer](https://hiddenlayer.com/innovation-hub/mcp-model-context-pitfalls-in-an-agentic-world/)
- [Implementing MCP: Tips, Tricks and Pitfalls — Nearform](https://nearform.com/digital-community/implementing-model-context-protocol-mcp-tips-tricks-and-pitfalls/)
- [AI Agent Interfaces in 2026: Filesystem vs API vs Database — Arize](https://arize.com/blog/agent-interfaces-in-2026-filesystem-vs-api-vs-database-what-actually-works/)
- [The Realities of Application Modernization with Agentic AI — Microsoft](https://devblogs.microsoft.com/all-things-azure/the-realities-of-application-modernization-with-agentic-ai-early-2026/)

---
*Pitfalls research for: AI agent planning system — GSD fork adapted for Djinn MCP*
*Researched: 2026-03-02*
