# Architecture Research

**Domain:** AI agent planning system (GSD fork adapted for MCP-based task/memory output)
**Researched:** 2026-03-02
**Confidence:** HIGH

## System Overview

```
┌──────────────────────────────────────────────────────────────────────┐
│                     USER ENTRY POINTS                                │
│  /djinn:new-project  /djinn:plan-phase  /djinn:discuss-phase         │
│  /djinn:progress                                                     │
├──────────────────────────────────────────────────────────────────────┤
│                     WORKFLOW LAYER (orchestrators)                    │
│  ┌──────────────┐ ┌──────────────┐ ┌──────────────┐ ┌──────────┐   │
│  │ new-project   │ │ plan-phase   │ │discuss-phase │ │ progress │   │
│  └──────┬───────┘ └──────┬───────┘ └──────┬───────┘ └────┬─────┘   │
│         │                │                │               │          │
├─────────┴────────────────┴────────────────┴───────────────┴──────────┤
│                     AGENT LAYER (subagents)                          │
│  ┌──────────────┐ ┌──────────────┐ ┌──────────────┐                 │
│  │ researchers   │ │ roadmapper   │ │   planner    │                 │
│  │ (4 parallel)  │ │              │ │              │                 │
│  └──────┬───────┘ └──────┬───────┘ └──────┬───────┘                 │
│         │                │                │                          │
│  ┌──────────────┐ ┌──────────────┐                                  │
│  │ synthesizer  │ │ plan-checker │                                   │
│  └──────┬───────┘ └──────┬───────┘                                  │
│         │                │                                           │
├─────────┴────────────────┴───────────────────────────────────────────┤
│                     MCP OUTPUT ADAPTER                               │
│  ┌──────────────────────────────────────────────────────────────┐    │
│  │  Djinn MCP Tool Calls (memory_write, task_create, etc.)     │    │
│  └──────────────────────────────────────────────────────────────┘    │
├──────────────────────────────────────────────────────────────────────┤
│                     DJINN SERVER (existing)                          │
│  ┌──────────────┐ ┌──────────────┐ ┌──────────────┐                 │
│  │   Memory     │ │    Tasks     │ │  Execution   │                 │
│  │  (knowledge  │ │   (kanban    │ │  (parallel   │                 │
│  │   base)      │ │    board)    │ │   agents)    │                 │
│  └──────────────┘ └──────────────┘ └──────────────┘                 │
└──────────────────────────────────────────────────────────────────────┘
```

### Component Responsibilities

| Component | Responsibility | Communicates With |
|-----------|----------------|-------------------|
| **Workflow Orchestrators** | Control the flow: questioning, research, requirements, roadmap, planning. Route between steps and spawn subagents. | User (interactive), Agent Layer (spawn), MCP Output Adapter (write artifacts) |
| **Agent Layer** | Perform focused work: research a dimension, create a roadmap, decompose a phase into tasks. Stateless -- receive context, produce output. | Workflows (spawned by), MCP Output Adapter (write results) |
| **MCP Output Adapter** | Translate GSD's artifact model to Djinn MCP calls. Maps briefs to `memory_write(type="brief")`, phases to `task_create(issue_type="epic")`, etc. | Agent Layer (receives artifact intent), Djinn Server (issues MCP calls) |
| **Djinn Server** | Existing system. Stores memory notes, manages task board, runs agent execution. Not modified by this project. | MCP Output Adapter (receives calls) |

## The Core Translation: Filesystem to MCP

GSD writes markdown files. Djinn stores typed notes and tasks. The central architectural question is: **where does the translation happen?**

### Option A: Translate at the agent boundary (each agent calls MCP directly)
Every agent (researcher, roadmapper, planner) replaces its `Write` tool calls with `memory_write` / `task_create` calls. Agents become MCP-aware.

### Option B: Translate at the workflow boundary (agents produce structured data, workflows translate)
Agents produce structured output (markdown or JSON). Workflows parse agent output and make MCP calls. Agents stay runtime-agnostic.

### Option C: Translate at a shared adapter layer (helper functions that agents call)
A thin adapter module provides functions like `save_research(title, content, tags)` that internally call MCP. Agents call the adapter instead of raw `Write` or raw MCP.

**Recommendation: Option A -- agents call MCP directly.** Because:

1. **Agents ARE prompts.** GSD agents are markdown files that become LLM prompts. They already contain tool-call instructions (`Write to: .planning/research/STACK.md`). Changing those instructions to `memory_write(type="research", ...)` is a direct substitution, not a layering problem.

2. **No runtime adapter exists.** There is no executable code layer between the LLM and the tools -- the LLM calls tools directly. An "adapter" would be another prompt section, which is just... the agent instructions.

3. **GSD's agent pattern is already tool-direct.** Researchers call `Write` to produce files. Replacing `Write` with `memory_write` is the same abstraction level. Adding an intermediate layer would be artificial complexity.

4. **Multi-runtime support comes from MCP itself.** All target runtimes (Claude Code, OpenCode, Gemini, Codex) can call MCP tools. The translation from "write this research" to `memory_write(...)` works identically across runtimes.

## Component Boundaries

### 1. Workflows (Orchestrators)

**What they are:** Markdown prompt templates that define multi-step flows with branching logic, subagent spawning, and user interaction.

**Boundary:** Workflows own the FLOW -- when to question, when to research, when to create a roadmap. They do NOT own the content of research/roadmap/plans.

**GSD workflows in scope (4):**

| Workflow | GSD Source | Djinn Version | Changes from GSD |
|----------|-----------|---------------|------------------|
| `new-project` | `workflows/new-project.md` | `workflows/new-project.md` | Replace `.planning/` file writes with memory_write calls. Replace gsd-tools.cjs init/commit calls with MCP equivalents. Remove STATE.md (Djinn task state replaces it). |
| `plan-phase` | `workflows/plan-phase.md` | `workflows/plan-phase.md` | Planner outputs `task_create` calls instead of PLAN.md files. Research stored in memory. Phase directory concept eliminated. |
| `discuss-phase` | `workflows/discuss-phase.md` | `workflows/discuss-phase.md` | CONTEXT.md replaced by `memory_write(type="design")`. Reads phase context from Djinn memory instead of filesystem. |
| `progress` | `workflows/progress.md` | `workflows/progress.md` | Reads state from `task_list` / `task_count` instead of STATE.md / gsd-tools.cjs. |

**Key change:** Every `node "$HOME/.claude/get-shit-done/bin/gsd-tools.cjs" ...` call must be replaced. GSD uses ~11 CJS modules for state management, roadmap parsing, phase tracking, etc. In Djinn, this state lives in the task board and memory. The Node.js tooling is eliminated entirely.

### 2. Agents (Subagents)

**What they are:** Focused LLM prompts that perform one job well. Spawned by workflows, receive context, produce output.

**Boundary:** Agents own CONTENT CREATION. A researcher produces research findings. A roadmapper produces phase structure. A planner produces task decompositions.

**GSD agents to fork (7):**

| Agent | Purpose | Output Change (GSD -> Djinn) |
|-------|---------|------------------------------|
| `project-researcher` (x4) | Investigate stack/features/architecture/pitfalls | `Write` file -> `memory_write(type="research")` |
| `research-synthesizer` | Merge 4 research dimensions into summary | `Write` SUMMARY.md -> `memory_write(type="research", tags=["synthesis"])` |
| `roadmapper` | Create phase structure from requirements | `Write` ROADMAP.md + STATE.md -> `memory_write(type="roadmap")` + create epics via `task_create` |
| `planner` | Decompose phase into executable plans | `Write` PLAN.md files -> `task_create` for each task with acceptance_criteria, design, blockers |
| `plan-checker` | Verify plans achieve phase goal | `Write` checker report -> `task_comment_add` on epic with findings |
| `phase-researcher` | Research specific phase implementation | `Write` RESEARCH.md -> `memory_write(type="research")` |

**Agents NOT forked (out of scope per PROJECT.md):**

| Agent | Why Excluded |
|-------|-------------|
| `gsd-executor` | Djinn execution engine replaces this |
| `gsd-verifier` | Djinn review pipeline replaces this |
| `gsd-debugger` | Deferred |
| `gsd-codebase-mapper` | Deferred |
| `gsd-integration-checker` | Deferred |

### 3. Installer (NPM Package)

**What it is:** A Node.js script that installs agent definitions and workflow templates into the correct locations for each runtime (Claude Code, OpenCode, Gemini, Codex).

**Boundary:** The installer owns DISTRIBUTION. It copies files, configures MCP connections, sets up slash commands. It does NOT contain planning logic.

**GSD installer pattern:** Single `bin/install.js` (84KB) that:
- Detects installed runtimes
- Copies agent files to `~/.claude/agents/` (Claude Code), `~/.opencode/agents/` (OpenCode), etc.
- Registers slash commands
- Manages project-level vs global install

**Djinn adaptation:**
- Same multi-runtime pattern
- Files go to runtime-specific locations
- Must configure MCP server connection (djinn-server must be running)
- Claude Code users get BOTH: plugin (skill) + NPM agents. Other runtimes get NPM agents only.

### 4. Skill (Claude Code Plugin)

**What it is:** The existing `plugin/skills/djinn/` directory that teaches Claude Code how to use Djinn's MCP tools.

**Boundary:** The skill owns TOOL USAGE PATTERNS. It does not own planning methodology -- that belongs to the workflows/agents.

**Integration:** The new planning workflows should be registered as additional skills or commands within the plugin. When a Claude Code user runs `/djinn:new-project`, it invokes the workflow from the skill directory.

**Current plugin structure:**
```
plugin/
├── .claude-plugin/
├── .mcp.json
├── hooks/
└── skills/
    └── djinn/
        ├── SKILL.md
        └── cookbook/
            ├── gsd.md              # Bridge pattern (THIS gets replaced)
            ├── execution-planning.md
            ├── memory-management.md
            ├── task-management.md
            ├── work-decomposition.md
            └── superpowers.md
```

**New structure adds planning workflows:**
```
plugin/
├── skills/
│   └── djinn/
│       ├── SKILL.md                # Updated: detect /djinn:* commands
│       ├── cookbook/                # Existing (unchanged)
│       └── workflows/              # NEW: planning methodology
│           ├── new-project.md
│           ├── plan-phase.md
│           ├── discuss-phase.md
│           └── progress.md
├── agents/                         # NEW: subagent definitions
│   ├── djinn-project-researcher.md
│   ├── djinn-roadmapper.md
│   ├── djinn-planner.md
│   ├── djinn-plan-checker.md
│   ├── djinn-phase-researcher.md
│   └── djinn-research-synthesizer.md
```

## Data Flow

### Flow 1: New Project (questioning -> research -> requirements -> roadmap)

```
User: /djinn:new-project
    │
    ▼
[new-project workflow]
    │
    ├─── Deep Questioning (interactive)
    │    │
    │    ▼
    │    memory_write(type="brief", content=PROJECT)
    │
    ├─── Spawn 4 Researchers (parallel)
    │    ├── Stack researcher ──► memory_write(type="research", title="Stack Research", tags=["stack"])
    │    ├── Features researcher ──► memory_write(type="research", title="Feature Research", tags=["features"])
    │    ├── Architecture researcher ──► memory_write(type="research", title="Architecture Research", tags=["architecture"])
    │    └── Pitfalls researcher ──► memory_write(type="research", title="Pitfalls Research", tags=["pitfalls"])
    │         │
    │         ▼
    │    Spawn Synthesizer
    │    └── memory_write(type="research", title="Research Summary", tags=["synthesis"])
    │
    ├─── Define Requirements (interactive with user)
    │    │
    │    ▼
    │    memory_write(type="requirement", title="V1 Requirements", content=REQUIREMENTS)
    │
    └─── Spawn Roadmapper
         │
         ▼
         memory_write(type="roadmap", content=ROADMAP)
         │
         ▼
         For each phase in roadmap:
           task_create(issue_type="epic", title="Phase N: {name}", ...)
```

### Flow 2: Plan Phase (context -> research -> planning -> verification)

```
User: /djinn:plan-phase 1
    │
    ▼
[plan-phase workflow]
    │
    ├─── Read context from memory
    │    memory_read("roadmap")
    │    memory_read("V1 Requirements")
    │    memory_search(query="Phase 1", type="design")  # CONTEXT.md equivalent
    │
    ├─── Spawn Phase Researcher (optional)
    │    └── memory_write(type="research", title="Phase 1 Implementation Research")
    │
    ├─── Spawn Planner
    │    │
    │    ▼
    │    For each plan in phase:
    │      task_create(
    │        issue_type="task",
    │        parent=phase_epic_id,
    │        title="...",
    │        design="...",
    │        acceptance_criteria=[...],
    │        labels=["wave:1"]
    │      )
    │    │
    │    ▼
    │    For wave dependencies:
    │      task_blockers_add(id=wave2_task, blocking_id=wave1_task)
    │
    └─── Spawn Plan Checker (optional)
         └── task_comment_add(id=phase_epic, body="[VERIFICATION] ...")
```

### Flow 3: Discuss Phase (interactive context capture)

```
User: /djinn:discuss-phase 1
    │
    ▼
[discuss-phase workflow]
    │
    ├─── Read phase context from Djinn
    │    task_show(id=phase_epic_id)
    │    memory_read("roadmap")
    │    memory_search(query="Phase 1")
    │
    ├─── Identify gray areas
    │
    ├─── Interactive discussion (multiple rounds)
    │
    └─── Save decisions
         memory_write(
           type="design",
           title="Phase 1 Design Decisions",
           content=CONTEXT,
           tags=["phase-1", "decisions"]
         )
```

### Flow 4: Progress Check

```
User: /djinn:progress
    │
    ▼
[progress workflow]
    │
    ├─── task_count(group_by="status")           # Overall progress
    ├─── task_list(issue_type="epic")             # Phase status
    ├─── task_list(status="in_progress")          # Active work
    ├─── task_ready(issue_type="!epic")           # What's next
    │
    └─── Route to next action
         ├── No epics yet ──► Suggest /djinn:new-project
         ├── Phase has no tasks ──► Suggest /djinn:plan-phase N
         ├── Phase has ready tasks ──► Suggest execution_start()
         └── Phase in review ──► Show review status
```

## Artifact Mapping (GSD Filesystem -> Djinn MCP)

| GSD Artifact | GSD Location | Djinn Equivalent | MCP Call |
|-------------|-------------|-----------------|----------|
| PROJECT.md | `.planning/PROJECT.md` | Brief (singleton) | `memory_write(type="brief")` |
| config.json | `.planning/config.json` | Reference note | `memory_write(type="reference", title="Planning Config")` |
| REQUIREMENTS.md | `.planning/REQUIREMENTS.md` | Requirement note | `memory_write(type="requirement")` |
| ROADMAP.md | `.planning/ROADMAP.md` | Roadmap (singleton) + Epics | `memory_write(type="roadmap")` + `task_create(issue_type="epic")` per phase |
| STATE.md | `.planning/STATE.md` | **Eliminated** | Task board IS the state. Use `task_count`, `task_list`. |
| STACK.md | `.planning/research/STACK.md` | Research note | `memory_write(type="research", tags=["stack"])` |
| FEATURES.md | `.planning/research/FEATURES.md` | Research note | `memory_write(type="research", tags=["features"])` |
| ARCHITECTURE.md | `.planning/research/ARCHITECTURE.md` | Research note | `memory_write(type="research", tags=["architecture"])` |
| PITFALLS.md | `.planning/research/PITFALLS.md` | Research note | `memory_write(type="research", tags=["pitfalls"])` |
| SUMMARY.md | `.planning/research/SUMMARY.md` | Research note | `memory_write(type="research", tags=["synthesis"])` |
| CONTEXT.md | `.planning/phases/{N}/CONTEXT.md` | Design note | `memory_write(type="design", tags=["phase-N"])` |
| RESEARCH.md | `.planning/phases/{N}/RESEARCH.md` | Research note | `memory_write(type="research", tags=["phase-N"])` |
| PLAN.md (x per phase) | `.planning/phases/{N}/*-PLAN.md` | Tasks under phase epic | `task_create(parent=epic_id, ...)` per plan |
| Phase directories | `.planning/phases/{NN}-{slug}/` | **Eliminated** | Epics + labels replace directory structure |

## Task Hierarchy Mapping

```
GSD Concept          Djinn Entity          Rationale
─────────────        ─────────────         ──────────
Milestone            Epic                  Top-level strategic container
Phase                Epic (child of        PROJECT.md says "Phases → Epics"
                     milestone epic)       but per Key Decisions, revisit.
                                           See note below.
Plan                 Task (child of        Each plan becomes one executable task
                     phase epic)           with acceptance_criteria and design
Wave ordering        Blocker               Wave 1 tasks block Wave 2 tasks
                     dependencies
```

**Note on Phase → Epic vs Feature mapping:** PROJECT.md lists a pending decision: "Phases → Epics not Features." The rationale is kanban visibility. However, this creates a 3-level epic hierarchy (Milestone Epic → Phase Epic → Tasks) which Djinn may not support natively. The alternative is Milestone → Epic, Phase → Feature, Plan → Task, which follows the standard hierarchy. **Recommendation:** Use the standard hierarchy -- Milestone as Epic, Phase as Feature, Plan as Task. This aligns with Djinn's `epic → feature → task` model documented in the work decomposition cookbook. If kanban visibility of phases is needed, use labels (`phase:1`, `phase:2`) for filtering.

## Patterns to Follow

### Pattern 1: Memory-First Context Loading

**What:** Before any agent does work, it loads context from Djinn memory rather than reading filesystem paths.
**When:** Every workflow step, every agent spawn.
**Why GSD does it differently:** GSD agents receive `<files_to_read>` blocks with filesystem paths. Djinn agents receive memory identifiers.

```
# GSD pattern (filesystem)
<files_to_read>
- .planning/PROJECT.md
- .planning/REQUIREMENTS.md
- .planning/research/SUMMARY.md
</files_to_read>

# Djinn pattern (MCP memory)
<context_loading>
memory_read(identifier="brief")
memory_read(identifier="V1 Requirements")
memory_search(query="research synthesis", type="research", limit=1)
</context_loading>
```

### Pattern 2: Structured Agent Output via MCP

**What:** Agents write results directly to Djinn via MCP calls instead of writing files.
**When:** Every agent that currently produces output files.
**Trade-off:** Agents are now coupled to Djinn MCP tool signatures. But since MCP is the mandatory backend (per PROJECT.md constraint), this coupling is acceptable.

```
# GSD pattern: researcher writes a file
Write(file_path=".planning/research/STACK.md", content="# Stack Research\n...")

# Djinn pattern: researcher writes to memory
memory_write(
  title="Stack Research",
  type="research",
  content="# Stack Research\n...",
  tags=["stack", "project-research"]
)
```

### Pattern 3: Roadmap-as-Data (Memory + Task Board)

**What:** The roadmap is both a narrative document (memory note) and a live structure (epics on the board).
**When:** During roadmap creation and progress checking.
**Why both:** The narrative captures rationale, success criteria, ordering logic. The task board captures live state -- what's done, what's blocked, what's in progress. They serve different purposes.

```
# Step 1: Write the narrative roadmap
memory_write(type="roadmap", content="# Roadmap\n## Phase 1: Auth\nGoal: ...\nSuccess criteria: ...")

# Step 2: Create the live structure
milestone_epic = task_create(issue_type="epic", title="V1 Milestone", emoji="🎯", ...)
phase1_feature = task_create(issue_type="feature", parent=milestone_epic, title="Phase 1: Auth", ...)
phase2_feature = task_create(issue_type="feature", parent=milestone_epic, title="Phase 2: Content", ...)

# Phase progress = task_count(parent=phase1_feature, group_by="status")
```

## Anti-Patterns to Avoid

### Anti-Pattern 1: Dual Storage

**What people do:** Write to both `.planning/` files AND Djinn memory "just in case."
**Why it's wrong:** Creates sync problems. Which is truth? Violates PROJECT.md constraint ("MCP-only, no file fallback").
**Do this instead:** Write to Djinn memory only. If you need local debugging, use `memory_read` to inspect.

### Anti-Pattern 2: Monolithic Agent Prompts

**What people do:** Put the entire workflow logic into a single agent prompt, making it 2000+ lines.
**Why it's wrong:** Exceeds context windows, degrades quality (GSD's quality degradation curve), impossible to test.
**Do this instead:** Keep the GSD separation -- small orchestrator workflows that spawn focused subagents. Each agent is 200-400 lines of prompt.

### Anti-Pattern 3: Reimplementing gsd-tools.cjs in Prompt Logic

**What people do:** Try to replicate the CJS tooling's state management, phase parsing, and init logic as inline prompt instructions.
**Why it's wrong:** GSD's tooling exists because filesystem state is hard to parse. Djinn's task board provides structured queries (task_list, task_count, task_show) that replace this need.
**Do this instead:** Replace gsd-tools.cjs calls with direct MCP queries. `gsd-tools.cjs init` becomes `task_list(issue_type="epic")` + `memory_catalog()`. `gsd-tools.cjs roadmap analyze` becomes `task_list(parent=milestone_epic)`.

### Anti-Pattern 4: Translating GSD's PLAN.md Format Literally

**What people do:** Create Djinn tasks with the full PLAN.md content crammed into the description field.
**Why it's wrong:** PLAN.md is a PROMPT for GSD executors. Djinn tasks are consumed by Djinn's own agent execution system, which uses `acceptance_criteria`, `design`, and `description` fields natively.
**Do this instead:** Decompose PLAN.md content into proper Djinn task fields:
- Plan objective -> task `title`
- Plan context -> task `description`
- Plan approach -> task `design`
- Plan success criteria -> task `acceptance_criteria` (array)
- Plan must_haves -> task `acceptance_criteria` (array)
- Plan wave -> `labels=["wave:N"]` + blocker dependencies

## Suggested Build Order

Dependencies flow downward -- each item depends on the ones above it.

```
Phase 1: Foundation (no dependencies)
├── Artifact mapping functions (how each GSD artifact becomes MCP calls)
├── Context loading pattern (memory_read/search replacing file reads)
└── Agent prompt adaptations (tool call substitutions)

Phase 2: Core Workflow - new-project (depends on Phase 1)
├── Questioning flow (interactive, minimal MCP -- just brief at the end)
├── Research spawning (4 parallel researchers writing to memory)
├── Research synthesis (synthesizer reading from memory)
├── Requirements definition (requirement note)
└── Roadmap creation (roadmap note + epic/feature task creation)

Phase 3: Core Workflow - plan-phase (depends on Phase 2)
├── Context loading from memory (replaces filesystem reads)
├── Phase researcher (memory-based)
├── Planner (creates tasks instead of PLAN.md files)
├── Plan checker (reads tasks, adds comments)
└── Wave dependency mapping (blocker relationships)

Phase 4: Supporting Workflows (depends on Phases 2-3)
├── discuss-phase (design note output)
└── progress (pure read -- task queries replace gsd-tools.cjs)

Phase 5: Distribution (depends on Phases 2-4)
├── Claude Code plugin integration (register workflows as slash commands)
└── NPM installer for other runtimes (OpenCode, Gemini, Codex)
```

**Rationale:**
- Phase 1 is pure translation work -- establishing the patterns everything else uses.
- Phase 2 (new-project) is the entry point. Without it, nothing else can run.
- Phase 3 (plan-phase) is the core loop. Once new-project creates a roadmap + epics, plan-phase creates the tasks that feed execution.
- Phase 4 is supporting. discuss-phase enriches planning quality but is not blocking. progress is pure reads.
- Phase 5 is packaging. The workflows must work before they can be distributed.

## What Gets Eliminated

| GSD Component | Why Eliminated | Replaced By |
|---------------|---------------|-------------|
| `bin/lib/*.cjs` (11 modules) | State management for filesystem artifacts | Djinn MCP queries (task_list, task_count, memory_read) |
| `gsd-tools.cjs` CLI | Init, commit, state, roadmap parsing | Direct MCP tool calls |
| `.planning/` directory | Filesystem storage | Djinn memory + task board |
| `STATE.md` | Session state tracking | Task board state (task_list with status filters) |
| `templates/*.md` (file templates) | File structure templates | Agent prompts contain output format directly (agents write to MCP, not files) |
| `config.json` (planning prefs) | Workflow configuration | Either a reference note in memory or prompt parameters |
| Phase directories | `{NN}-{slug}/` artifact grouping | Epics (hierarchy) + labels (filtering) + memory tags |
| Git commit management | `gsd-tools.cjs commit` | Djinn memory is auto-versioned by git; tasks don't need commits |

## Integration Points

### External: Djinn MCP Server

| Interaction | MCP Tools Used | Notes |
|-------------|---------------|-------|
| Store planning artifacts | `memory_write`, `memory_edit` | Brief, roadmap, requirements, research, design notes |
| Read planning context | `memory_read`, `memory_search`, `memory_catalog` | Every workflow and agent needs this |
| Create work items | `task_create`, `task_update`, `task_blockers_add` | Roadmap creates epics/features; planner creates tasks |
| Check progress | `task_list`, `task_count`, `task_ready` | Progress workflow, plan-phase init |
| Annotate work | `task_comment_add` | Plan checker findings, progress notes |

### Internal: Workflow-Agent Protocol

Workflows spawn agents with structured prompts. The contract:

**Workflow provides:**
- Objective (what the agent should accomplish)
- Memory identifiers to read (replaces `<files_to_read>`)
- Output instructions (which MCP calls to make)
- Quality gates (what makes the output acceptable)

**Agent returns:**
- Structured status (`## RESEARCH COMPLETE`, `## PLANNING COMPLETE`, etc.)
- Key findings summary
- Confidence level
- Any blockers encountered

This protocol is preserved from GSD with minimal changes -- the spawn mechanism and return format are runtime-agnostic.

## Sources

- GSD source code at `/home/fernando/git/references/get-shit-done` -- direct analysis of 34 workflows, 11 agents, 11 CJS modules (HIGH confidence)
- Djinn MCP tool signatures -- direct analysis of SKILL.md and cookbooks (HIGH confidence)
- PROJECT.md planning document -- project requirements and constraints (HIGH confidence)
- Djinn plugin structure -- direct analysis of `plugin/` directory (HIGH confidence)

---
*Architecture research for: Djinn Planning System (GSD fork)*
*Researched: 2026-03-02*
