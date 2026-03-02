# Djinn Planning System

## What This Is

A fork of GSD (Get Shit Done) adapted for Djinn's MCP-based memory and task systems. It preserves GSD's planning methodology — deep questioning, parallel research, structured requirements, phased roadmaps — but replaces filesystem storage with Djinn memory and creates Djinn tasks directly instead of writing plan files. Multi-runtime compatible (Claude Code, OpenCode, Gemini, Codex).

## Core Value

GSD's planning methodology outputs directly into Djinn's memory and task systems, eliminating the bridge between planning and execution.

## Requirements

### Validated

(None yet — ship to validate)

### Active

- [ ] Fork GSD's planning workflows adapted for Djinn MCP
- [ ] `/djinn:new-project` — questioning → research → requirements → roadmap, all stored in Djinn memory
- [ ] `/djinn:plan-phase` — phase planning that creates Djinn tasks (phases → epics, plans → tasks)
- [ ] `/djinn:discuss-phase` — phase context gathering before planning
- [ ] `/djinn:progress` — check project state and route to next action
- [ ] Parallel research agents (project-researcher, phase-researcher, research-synthesizer) adapted for Djinn memory output
- [ ] Memory mapping: brief (PROJECT), requirement (REQUIREMENTS), roadmap (ROADMAP), research notes
- [ ] Task mapping: phases become epics, plans become tasks with blocker dependencies for wave ordering
- [ ] Multi-runtime installer (NPM) for OpenCode, Gemini, Codex
- [ ] Claude Code plugin distribution (skills in plugin/ directory)
- [ ] MCP-first storage — all planning artifacts go to Djinn memory, no `.planning/` files
- [ ] Command namespace: `/djinn:*` commands (new-project, plan-phase, discuss-phase, progress)

### Out of Scope

- Execution workflows (execute-phase, execute-plan) — Djinn orchestrator owns execution
- Verification workflows (verify-work, verify-phase) — Djinn review pipeline handles this
- GSD state management tooling (gsd-tools.cjs, STATE.md) — replaced by Djinn task state
- `.planning/` directory filesystem storage — replaced entirely by Djinn memory
- Milestone management (complete-milestone, new-milestone, audit-milestone) — defer to later
- Administrative workflows (add-phase, insert-phase, remove-phase, cleanup) — defer to later
- Debug workflow — defer to later
- Todo tracking (add-todo, check-todos) — Djinn tasks replace this
- Pause/resume workflows — defer to later

## Context

**Source codebase:** GSD v1.22.0 at `/home/fernando/git/references/get-shit-done`
- 34 workflow templates in `get-shit-done/workflows/`
- 11 agent definitions in `agents/`
- Node.js tooling in `get-shit-done/bin/lib/` (11 CJS modules)
- Multi-runtime installer in `bin/install.js`

**Target system:** Djinn MCP provides three integrated systems:
- **Memory** — persistent knowledge base with typed notes (adr, pattern, research, requirement, reference, design, brief, roadmap), wikilinks, FTS5 search
- **Tasks** — kanban board with epic/feature/task/bug hierarchy, blocker dependencies, status transitions
- **Execution** — parallel agent orchestration with review pipeline, worktree isolation

**Current integration:** A cookbook (`plugin/skills/djinn/cookbook/gsd.md`) describes a bridge pattern where GSD plans in `.planning/` files, then manually imports into Djinn. This project eliminates that bridge.

**Key GSD concepts to preserve:**
- Deep questioning methodology (collaborative, thread-following, not checklist)
- Parallel research with 4 dimension agents + synthesizer
- REQ-ID based requirements with traceability
- Phase-based roadmaps with success criteria
- Wave-based plan ordering within phases

**Djinn MCP memory type mapping:**
| GSD Artifact | Djinn Memory Type | Notes |
|---|---|---|
| PROJECT.md | `brief` | Singleton — one per project |
| REQUIREMENTS.md | `requirement` | REQ-IDs preserved in content |
| ROADMAP.md | `roadmap` | Singleton — one per project |
| research/STACK.md | `research` | Tagged with dimension |
| research/FEATURES.md | `research` | Tagged with dimension |
| research/ARCHITECTURE.md | `research` | Tagged with dimension |
| research/PITFALLS.md | `research` | Tagged with dimension |
| research/SUMMARY.md | `research` | Synthesized findings |
| Phase plans | Djinn tasks | Created via task_create |
| config.json | `reference` | Workflow preferences |

**Djinn task hierarchy mapping:**
| GSD Concept | Djinn Entity |
|---|---|
| Milestone | Epic |
| Phase | Feature (child of epic) |
| Plan/Task | Task (child of feature) |
| Wave ordering | Blocker dependencies |

## Constraints

- **MCP Required**: All runtimes need djinn-server running — no filesystem fallback
- **Multi-Runtime**: Must support Claude Code, OpenCode, Gemini, Codex (preserve GSD's installer pattern)
- **Distribution**: Plugin for Claude Code users, NPM installer for other runtimes
- **Upstream Compatibility**: GSD is actively developed — fork structure should allow pulling upstream methodology improvements
- **Command Namespace**: `/djinn:*` to avoid conflicts with installed GSD

## Key Decisions

| Decision | Rationale | Outcome |
|----------|-----------|---------|
| Fork GSD, don't wrap it | Wrapping adds a bridge layer; forking lets us deeply integrate with Djinn MCP | — Pending |
| MCP-only, no file fallback | Simplifies architecture; Djinn MCP is the source of truth | — Pending |
| Phases → Epics not Features | Epics give better visibility in kanban; features are too granular for phases | — Pending |
| v1 = planning + research only | Get the core loop right before adding admin/lifecycle workflows | — Pending |
| Preserve multi-runtime support | Key differentiator over pure Claude Code solutions | — Pending |

---
*Last updated: 2026-03-02 after initialization*
