# Feature Research

**Domain:** AI agent planning system with MCP integration (fork of GSD methodology targeting Djinn memory/task/execution)
**Researched:** 2026-03-02
**Confidence:** HIGH (direct access to both GSD source and Djinn target systems, plus competitive landscape analysis)

## Feature Landscape

### Table Stakes (Users Expect These)

Features users assume exist. Missing these = the planning system feels broken or incomplete.

| Feature | Why Expected | Complexity | Notes |
|---------|--------------|------------|-------|
| **Deep questioning / context gathering** | GSD's discuss-phase is what separates it from "just prompting." Users expect the system to extract implementation decisions before planning. Without this, plans are generic. | MEDIUM | Port GSD's discuss-phase methodology. Output goes to Djinn memory (type=`design` or `research`) instead of CONTEXT.md files. Gray area identification, scope guardrailing, deferred ideas capture must all survive the port. |
| **Parallel research with dimension agents** | GSD spawns 4 parallel researchers (stack, features, architecture, pitfalls) + synthesizer. This is the core of how GSD produces informed plans rather than hallucinated ones. Users who adopt GSD specifically value this. | HIGH | 4 dimension agents + synthesizer must write to Djinn memory (type=`research`, tagged by dimension). Synthesizer reads all 4 results and produces the summary. Key challenge: coordinating parallel agent outputs into Djinn memory without conflicts. |
| **Requirements definition with REQ-IDs** | Traceability from requirements to phases to tasks to commits. GSD's REQUIREMENTS.md with REQ-IDs is how users verify nothing was missed. | MEDIUM | Maps to Djinn memory type=`requirement`. REQ-IDs are content conventions, not system features -- they live in the markdown body. Requirements must link to roadmap phases and downstream tasks via wikilinks or memory_refs. |
| **Phased roadmap generation** | Breaking a project into ordered phases with goals, success criteria, and dependencies. This is the primary output of new-project. | MEDIUM | Maps to Djinn memory type=`roadmap` (singleton). Phases map to Djinn epics. The roadmap note must be rich enough that plan-phase can read it and know exactly what each phase delivers. |
| **Phase planning (plans from roadmap phases)** | Given a roadmap phase, produce executable task definitions. GSD's plan-phase creates PLAN.md files; Djinn equivalent creates tasks directly. | HIGH | This is the most complex port. GSD's plan-phase orchestrates: phase-researcher, planner, plan-checker (with revision loop up to 3 iterations). Output must be Djinn tasks with acceptance_criteria, design fields, blocker dependencies for wave ordering, and labels for grouping. |
| **Wave-based task ordering** | Plans within a phase are grouped into waves (independent = parallel, dependent = sequential). This is how GSD handles parallelism within a phase. | LOW | Maps directly to Djinn's blocker dependency system. Wave 1 tasks have no blockers. Wave 2 tasks are blocked_by Wave 1 tasks. Djinn execution engine already respects this. |
| **Project brief / context persistence** | A single source of truth for "what is this project." GSD's PROJECT.md. | LOW | Maps to Djinn memory type=`brief` (singleton). Written during new-project, read by all downstream workflows. |
| **Progress awareness and routing** | "Where am I and what should I do next?" GSD's progress command reads state and routes to the right action. | MEDIUM | Must query Djinn task/execution state instead of reading STATE.md files. Progress checks: task counts by status, active execution phases, next unplanned epic, and routes to discuss-phase or plan-phase accordingly. |
| **Work decomposition (epic > feature > task)** | Structured hierarchy so work is tractable for agents. Each level has clear sizing guidance (epic=weeks, feature=2-4h, task=1 outcome). | LOW | Already native to Djinn's task system. The planning system just needs to create tasks at the right level with correct parent relationships. Djinn enforces the hierarchy. |
| **Acceptance criteria on tasks** | Observable, testable outcomes for each piece of work. Without these, agents produce untestable work and review is subjective. | LOW | Djinn's `acceptance_criteria` array field exists. The planning system must produce well-structured AC (Given/When/Then or observable outcomes) during plan-phase. |
| **Multi-runtime support** | GSD supports Claude Code, OpenCode, Gemini, Codex. Users on non-Claude runtimes need this. | MEDIUM | Preserve GSD's NPM installer pattern for non-Claude runtimes. Claude Code users get the plugin distribution. MCP tools are runtime-agnostic by design, so the methodology port is the main work. |

### Differentiators (Competitive Advantage)

Features that set this apart from raw GSD, Kiro, BMAD, cc-sdd, and other planning tools.

| Feature | Value Proposition | Complexity | Notes |
|---------|-------------------|------------|-------|
| **MCP-native storage (no filesystem bridge)** | GSD writes to `.planning/` files, then users manually import into their execution system. Djinn planning writes directly to memory and creates tasks directly. Zero import step. This eliminates the biggest friction point in the current GSD-Djinn bridge cookbook. | MEDIUM | The entire point of this fork. Every artifact goes to Djinn memory_write or task_create. No `.planning/` directory. No STATE.md. No manual import. The bridge cookbook becomes unnecessary. |
| **Persistent knowledge base across sessions** | GSD's `.planning/` files die when context resets. Djinn memory persists with full-text search, wikilinks, and version history. Research from 3 months ago is instantly findable. Competitors (Kiro, BMAD, cc-sdd) all use filesystem -- one bad `git clean` wipes planning state. | LOW | Already exists in Djinn. The differentiator is that planning workflows write to it natively. Research agents write memory notes. Requirements link to tasks. ADRs link to implementation. The knowledge graph grows automatically through planning. |
| **Bidirectional memory-task linking** | Tasks reference memory notes (ADRs, patterns, research). Memory notes can look up which tasks reference them. When reviewing a task, you can trace why a decision was made. When reading research, you can see what work it spawned. | LOW | Djinn already supports `memory_refs` on tasks and `memory_task_refs()` for reverse lookup. The planning system just needs to set these links during task creation. |
| **Integrated execution pipeline** | GSD plans, then you have to figure out how to execute. Djinn planning creates tasks that the Djinn execution engine runs immediately -- parallel agents, worktree isolation, review pipeline, auto-merge. Plan-to-running-code in one system. | LOW | Already exists in Djinn. The differentiator is that planning creates tasks shaped for execution (right acceptance criteria, right design field, right blocker dependencies) so execution_start just works. |
| **Research persistence with knowledge graph** | Research agents write typed memory notes with wikilinks. Stack research links to architecture research. Architecture research links to ADRs. Over multiple milestones, the knowledge graph accumulates institutional knowledge that informs future planning. | MEDIUM | Requires research agents to produce well-linked notes. The synthesizer should create cross-references between dimension outputs. Each research note should link to related requirements and ADRs. |
| **Context-efficient orchestration** | GSD's orchestrators can consume 30-50% of context managing state. Djinn MCP calls are lightweight (send task_id, get status back). The orchestrator stays lean, delegates to subagents with full context windows, and queries Djinn for state instead of reading files. | MEDIUM | Orchestrator workflows should use MCP calls for state management, not file reads. Phase status comes from `execution_phase_list()`, not parsing ROADMAP.md. Task counts come from `task_count()`, not grepping files. This keeps orchestrator context available for actual planning. |
| **Human-in-the-loop checkpoints via task state** | GSD uses `autonomous: false` flags on plans. Djinn uses task status transitions and comments. Block a task, add a comment explaining what decision is needed, human responds via comment, agent unblocks. Richer than a boolean flag -- the full conversation is preserved on the task. | LOW | Already supported in Djinn. Planning system creates checkpoint tasks with appropriate labels and blocking transitions. The comment thread on the task becomes the decision record. |
| **Automatic review pipeline** | GSD has a verify-work workflow for manual UAT. Djinn has an automated task review and phase review pipeline with reject/approve transitions. Review feedback goes back to the agent as task comments, creating a tight revision loop. | LOW | Already exists in Djinn execution. Planning just needs to create tasks with enough acceptance criteria that automated review can verify against them. |
| **Cross-project knowledge reuse** | Djinn memory supports multiple projects. Patterns discovered in Project A can be referenced in Project B's planning via memory_search. A "use PostgreSQL for OLTP" ADR from one project can inform another project's stack research. | LOW | Djinn already supports multi-project memory. Planning agents should search existing memory before researching from scratch. "Have we solved this before?" becomes a first-class step in research. |
| **Structured task fields (design, acceptance_criteria)** | GSD embeds everything in PLAN.md prose. Djinn has dedicated `design` and `acceptance_criteria` fields. This means: design decisions are searchable, acceptance criteria are structured arrays, and review can programmatically check criteria. | LOW | Already in Djinn. The planning system's planner agent must decompose plan content into the right fields rather than dumping everything into description. |

### Anti-Features (Deliberately NOT Building)

Features to explicitly NOT build because Djinn already handles them, or because they add complexity without value.

| Feature | Why Requested | Why NOT Building | What to Do Instead |
|---------|---------------|-----------------|-------------------|
| **Filesystem state management (STATE.md, config.json)** | GSD needs these to track position, preferences, and accumulated context across sessions. | Djinn memory and task state replace all of this. Task status tells you position. Memory stores preferences. Task comments store accumulated context. Building a parallel state system creates sync problems. | Use `task_list(status="in_progress")` for position. Use memory type=`reference` for project preferences. Use task comments for accumulated context. |
| **Execution orchestration** | GSD's execute-phase and execute-plan workflows manage subagent spawning, wave execution, checkpoint handling, and result collection. | Djinn's execution engine already does all of this with better infrastructure (worktree isolation, review pipeline, branch management, pause/resume, kill). Re-implementing would be worse. | Use `execution_start()` after planning creates tasks. Djinn coordinator handles everything. |
| **Verification workflows** | GSD's verify-work and verify-phase provide manual UAT and automated checking. | Djinn's review pipeline (task_review + phase_review transitions) provides structured review with reject/approve/comment. The revision loop is built into execution. | Tasks created by planning include strong acceptance criteria. Djinn's review pipeline validates against them. |
| **Git branching strategy management** | GSD manages branch creation, checkout, and merge strategies per phase. | Djinn execution engine handles all git operations -- branch provisioning per phase, worktree creation per task, merge after approval. | Planning just creates tasks. Execution handles git. |
| **GSD tooling (gsd-tools.cjs)** | GSD's 11 CJS modules manage roadmap parsing, state snapshots, phase-plan indexing, progress bars, summary extraction, and commit helpers. | These tools exist to manage filesystem state that we're replacing with MCP calls. Porting them means maintaining a parallel state system. | MCP calls replace every function: `task_count()` for progress, `task_list()` for status, `memory_read()` for artifacts, `execution_phase_list()` for phase state. |
| **Milestone lifecycle management** | GSD has complete-milestone, new-milestone, audit-milestone for multi-milestone projects. | Out of scope for v1. The core loop (new-project -> discuss -> plan -> execute) must work first. Milestone management adds lifecycle complexity that isn't needed until projects actually complete their first milestone. | Defer. Can be added as v1.x when the core loop is validated. |
| **Administrative phase manipulation** | GSD's add-phase, insert-phase, remove-phase for modifying the roadmap after creation. | Djinn provides `execution_phase_create`, `execution_phase_delete`, `execution_phase_update` for runtime phase management. Roadmap edits go through memory_edit on the roadmap note. No special workflow needed. | Use Djinn's phase management MCP calls directly. Edit roadmap memory note. |
| **Todo tracking** | GSD's add-todo, check-todos for tracking small items between sessions. | Djinn tasks replace this entirely. A todo is just a task with high priority and no parent. | Use `task_create(issue_type="task", priority=0)` for urgent items. |
| **Pause/resume workflow** | GSD's pause-work and resume-project serialize/deserialize session state. | Djinn tasks persist. Memory persists. Execution can `pause()` and `resume()`. There's no state to serialize -- just query current state on resume. | Run progress command on resume. It queries Djinn state and routes to the right action. |
| **Real-time collaboration / multi-user** | "Multiple developers planning simultaneously." | Adds massive complexity (conflict resolution, locking, real-time sync). Single-user planning with shared execution output is sufficient. The planning methodology is inherently single-session (one conversation with the user). | One user plans. Djinn execution parallelizes across agents. Results are shared via git branches. |
| **Visual planning UI** | "Drag-and-drop Gantt chart for phase ordering." | CLI/conversation-first tool. Users interact through slash commands and natural language. Building a UI duplicates Djinn's desktop kanban board. | Djinn desktop shows the kanban board. Planning happens in conversation. Phase editor in Djinn desktop handles visual phase arrangement. |

## Feature Dependencies

```
[Project Brief (new-project)]
    |
    v
[Deep Questioning (discuss-phase)]
    |
    +---requires---> [Research Persistence (memory)]
    |
    v
[Parallel Research (4 dimensions + synthesizer)]
    |
    +---requires---> [Research Persistence (memory)]
    +---requires---> [Multi-runtime support (for agent spawning)]
    |
    v
[Requirements Definition (REQ-IDs)]
    |
    +---requires---> [Project Brief]
    +---enhances---> [Research Persistence] (links reqs to research)
    |
    v
[Phased Roadmap Generation]
    |
    +---requires---> [Requirements Definition]
    +---requires---> [Research Summary]
    |
    v
[Phase Planning (plan-phase)]
    |
    +---requires---> [Phased Roadmap]
    +---requires---> [Deep Questioning] (CONTEXT for the phase)
    +---requires---> [Work Decomposition] (knows how to size tasks)
    +---requires---> [Wave-based Ordering] (sets blocker dependencies)
    |
    v
[Progress Awareness]
    |
    +---requires---> [Roadmap + Task State] (queries both for position)
    +---routes-to--> [discuss-phase OR plan-phase OR execution_start]
```

### Dependency Notes

- **Research requires memory persistence:** Research agents must write to Djinn memory. Without memory_write, research results are lost between sessions, defeating the purpose of the fork.
- **Requirements require project brief:** The brief establishes project scope. Requirements are scoped to that scope. Without a brief, requirements have no boundary.
- **Roadmap requires both requirements and research:** The roadmap phases are derived from requirements (what to build) informed by research (how hard, what order). Missing either produces a worse roadmap.
- **Phase planning requires everything upstream:** discuss-phase context, research results, requirements, and roadmap phase definition all feed into plan-phase. This is the most dependent feature.
- **Progress awareness requires task and execution state:** It queries Djinn for task counts, execution status, and roadmap content to determine position and routing.
- **MCP-native storage is a cross-cutting dependency:** Every feature depends on writing to Djinn memory/tasks instead of files. If MCP storage doesn't work, nothing works.

## MVP Definition

### Launch With (v1)

Minimum viable: the core planning loop works end-to-end through Djinn MCP.

- [x] **Project brief creation (new-project start)** -- questioning methodology, writes to Djinn memory type=`brief`
- [x] **Deep questioning (discuss-phase)** -- gray area identification, decision capture, output to Djinn memory
- [x] **Parallel research (4 dimensions + synthesizer)** -- writes to Djinn memory type=`research`
- [x] **Requirements definition** -- REQ-IDs in Djinn memory type=`requirement`
- [x] **Roadmap generation** -- phases as Djinn epics, roadmap as memory type=`roadmap`
- [x] **Phase planning (plan-phase)** -- creates Djinn tasks with AC, design, blocker dependencies
- [x] **Progress awareness** -- queries Djinn state, routes to next action
- [x] **Multi-runtime support** -- Claude Code plugin + NPM installer for OpenCode, Gemini, Codex

### Add After Validation (v1.x)

Features to add once the core loop is proven.

- [ ] **Milestone lifecycle** -- complete-milestone, new-milestone when projects actually finish phases
- [ ] **Quick task mode** -- ad-hoc tasks with planning guarantees, outside the phase structure
- [ ] **Cross-project knowledge reuse** -- research agents search existing memory across projects before external research
- [ ] **Research revision loop** -- plan-checker equivalent that validates task quality before execution (GSD's 3-iteration revision loop)
- [ ] **PRD express path** -- import an existing PRD document and generate plans from it
- [ ] **Brownfield codebase mapping** -- map-codebase equivalent for existing projects

### Future Consideration (v2+)

Features to defer until product-market fit is established.

- [ ] **Audit/health workflows** -- audit-milestone, health, diagnose-issues equivalents
- [ ] **Administrative phase manipulation** -- add-phase, insert-phase, remove-phase equivalents (use Djinn MCP directly for now)
- [ ] **Settings/profile management** -- set-profile, settings equivalents (use Djinn settings_save for now)
- [ ] **Test planning workflow** -- add-tests equivalent for generating test suites from acceptance criteria
- [ ] **Transition workflow** -- GSD's transition for switching between project contexts

## Feature Prioritization Matrix

| Feature | User Value | Implementation Cost | Priority |
|---------|------------|---------------------|----------|
| MCP-native storage (no files) | HIGH | MEDIUM | P1 |
| Deep questioning (discuss-phase) | HIGH | MEDIUM | P1 |
| Parallel research (4 agents) | HIGH | HIGH | P1 |
| Requirements definition (REQ-IDs) | HIGH | MEDIUM | P1 |
| Roadmap generation | HIGH | MEDIUM | P1 |
| Phase planning (creates Djinn tasks) | HIGH | HIGH | P1 |
| Progress awareness + routing | HIGH | MEDIUM | P1 |
| Multi-runtime support | MEDIUM | MEDIUM | P1 |
| Project brief creation | HIGH | LOW | P1 |
| Wave-based ordering (blockers) | HIGH | LOW | P1 |
| Work decomposition guidance | MEDIUM | LOW | P1 |
| Research persistence (knowledge graph) | MEDIUM | LOW | P1 |
| Bidirectional memory-task linking | MEDIUM | LOW | P2 |
| Cross-project knowledge reuse | MEDIUM | LOW | P2 |
| Research revision loop (plan-checker) | MEDIUM | MEDIUM | P2 |
| Milestone lifecycle | MEDIUM | MEDIUM | P2 |
| Quick task mode | LOW | MEDIUM | P2 |
| PRD express path | MEDIUM | LOW | P2 |
| Brownfield codebase mapping | LOW | HIGH | P3 |
| Audit/health workflows | LOW | MEDIUM | P3 |
| Test planning workflow | LOW | MEDIUM | P3 |

**Priority key:**
- P1: Must have for launch -- the core planning loop
- P2: Should have, add after core loop is validated
- P3: Nice to have, future consideration

## Competitor Feature Analysis

| Feature | GSD (raw) | Kiro (AWS) | BMAD | cc-sdd | Djinn Planning (ours) |
|---------|-----------|------------|------|--------|----------------------|
| Questioning methodology | Deep discuss-phase with gray areas, scope guardrails | Requirements from NL prompt in EARS notation | Product Manager agent gathers requirements | Not present (assumes specs exist) | Port GSD's discuss-phase, output to Djinn memory |
| Research agents | 4 parallel dimension agents + synthesizer | Analyzes codebase for design | 26 specialized agents (overkill for most projects) | Not present | Port GSD's 4+1 pattern, output to Djinn memory |
| Requirements traceability | REQ-IDs linking reqs -> phases -> plans -> commits | Requirements as EARS specs, linked to tasks | PRD -> epic sharding -> story files | specs -> design -> tasks (linear) | REQ-IDs in memory, linked to epics/tasks via wikilinks |
| Planning output | PLAN.md files (filesystem) | Implementation tasks (filesystem) | Story files (filesystem) | Task lists (filesystem) | Djinn tasks (MCP) with structured fields |
| Persistent storage | `.planning/` files (volatile) | IDE workspace files | `.bmad/` directory (volatile) | Spec files in repo | Djinn memory (persistent, searchable, versioned) |
| Execution integration | Separate execute-phase workflow | Agent hooks for automation | Developer agent executes | Manual | Direct -- planning creates tasks, execution_start runs them |
| Review pipeline | verify-work (manual UAT) | Not integrated | QA agent | Not integrated | Djinn review pipeline (automated task + phase review) |
| Multi-runtime | Claude Code, OpenCode, Gemini, Codex | Kiro IDE only | Claude Code primary | Multiple (claims) | Claude Code plugin + NPM for OpenCode, Gemini, Codex |
| Knowledge graph | None (files are flat) | None | None | None | Djinn memory with wikilinks, FTS5 search, bidirectional task refs |
| Context efficiency | 30-50% context for state management | IDE-integrated (low overhead) | Heavy orchestrator context | Low overhead (simple specs) | MCP calls for state (minimal context consumption) |

## Sources

- GSD v1.22.0 source at `/home/fernando/git/references/get-shit-done/` -- 34 workflow files, 11 agent definitions (HIGH confidence, direct access)
- Djinn MCP system -- SKILL.md, all 6 cookbooks, full MCP tool API (HIGH confidence, direct access)
- [Kiro spec-driven development](https://kiro.dev/) -- AWS SDD features, EARS notation, agent hooks (MEDIUM confidence, web research)
- [BMAD Method](https://github.com/bmad-code-org/BMAD-METHOD) -- 26 agents, 68 workflows, scale-adaptive planning (MEDIUM confidence, web research)
- [cc-sdd](https://github.com/gotalab/cc-sdd) -- Kiro-style SDD for multiple runtimes (MEDIUM confidence, web research)
- [Claude Code Agent Teams](https://code.claude.com/docs/en/agent-teams) -- multi-agent coordination, task claiming, shared context (MEDIUM confidence, web research)
- [GSD community coverage](https://ccforeveryone.com/gsd) -- real-world usage patterns, 23-plan project reports (LOW confidence, community reports)
- [AI Agent Orchestration Comparison](https://www.morphllm.com/ai-coding-agent) -- 2026 tool comparison (MEDIUM confidence, web research)

---
*Feature research for: AI agent planning system (Djinn GSD fork)*
*Researched: 2026-03-02*
