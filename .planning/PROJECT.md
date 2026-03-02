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
- [ ] `/djinn:plan-milestone` — milestone planning that creates domain-structured Djinn tasks with blocker dependencies
- [ ] `/djinn:discuss-milestone` — milestone context gathering before planning
- [ ] `/djinn:progress` — check project state and route to next action
- [ ] Parallel research agents (project-researcher, milestone-researcher, research-synthesizer) adapted for Djinn memory output
- [ ] Memory mapping: brief (PROJECT), requirement (REQUIREMENTS), roadmap (ROADMAP), research notes
- [ ] Task creation: domain-structured epics and features, sequenced via blocker dependencies (roadmap milestones are narrative, not task hierarchy)
- [ ] Multi-runtime installer (NPM) for OpenCode, Gemini, Codex
- [ ] Claude Code plugin distribution (skills in plugin/ directory)
- [ ] MCP-first storage — all planning artifacts go to Djinn memory, no `.planning/` files
- [ ] Command namespace: `/djinn:*` commands (new-project, plan-milestone, discuss-milestone, progress)

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

**Planning → Djinn mapping:**
| Planning Concept | Djinn Representation | Notes |
|---|---|---|
| Roadmap milestones | Memory note (type=roadmap) | Narrative — describes goals, success criteria, sequencing |
| Epics | Djinn epics (domain-structured) | e.g., "User Authentication System", not "Milestone 1" |
| Features | Djinn features (deliverables) | 2-4h units under epics |
| Tasks | Djinn tasks (implementation) | One commit, one outcome |
| Milestone sequencing | Blocker dependencies | Features in milestone 2 blocked by milestone 1 features |
| Wave ordering | Blocker dependencies | Same mechanism, finer-grained |
| Execution grouping | Djinn execution phases | Auto-generated from ready tasks — NOT the same as roadmap milestones |

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
| MCP-only, no file fallback | Simplifies architecture; Djinn MCP is the source of truth. See Artifact Mapping reference note. | Accepted |
| Roadmap milestones are narrative, not task entities | See ADR-001: Hierarchy Mapping in Djinn memory. Domain-structured epics, features, and tasks. Milestones are narrative in roadmap note. | Accepted |
| Rename GSD "phases" to "milestones" | Avoids confusion with Djinn execution phases. See ADR-001 disambiguation table. | Accepted |
| v1 = planning + research only | Get the core loop right before adding admin/lifecycle workflows | — Pending |
| Preserve multi-runtime support | Key differentiator over pure Claude Code solutions | — Pending |
| All progress derived from live queries | No stored state notes. See ADR-002: State Derivation. Roadmap note is immutable. | Accepted |

---
*Last updated: 2026-03-02 after Phase 0 architecture decisions*
