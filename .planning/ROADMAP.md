# Roadmap: Djinn Planning System

## Overview

This roadmap delivers GSD's planning methodology adapted for Djinn's MCP-based memory and task systems. The journey starts with resolving architectural ambiguities that would infect every workflow (Phase 0), establishes the skill scaffolding that all workflows live inside (Phase 1), then delivers the two core workflows in dependency order -- new-project (Phase 2) produces the artifacts that plan-phase (Phase 3) consumes. Supporting workflows add depth to the planning loop (Phase 4), and distribution packages everything for multi-runtime consumption (Phase 5). The dependency chain is strictly linear: each phase builds on the previous.

## Phases

**Phase Numbering:**
- Integer phases (0, 1, 2, 3, 4, 5): Planned milestone work
- Decimal phases (2.1, 2.2): Urgent insertions (marked with INSERTED)

Decimal phases appear between their surrounding integers in numeric order.

- [ ] **Phase 0: Architecture Decisions** - Resolve hierarchy mapping and state derivation ambiguities before any workflow code
- [ ] **Phase 1: Skill Scaffolding** - Establish SKILL.md structure, directory layout, and shared MCP patterns
- [ ] **Phase 2: Core Workflow -- new-project** - Full project initialization flow writing to Djinn memory and task board
- [ ] **Phase 3: Core Workflow -- plan-milestone** - Milestone planning that creates Djinn tasks with wave ordering
- [ ] **Phase 4: Supporting Workflows** - discuss-milestone for context gathering and design decision capture
- [ ] **Phase 5: Distribution** - Claude Code plugin extension and NPM multi-runtime installer

## Phase Details

### Phase 0: Architecture Decisions
**Goal**: Load-bearing architectural ambiguities are resolved and documented so every subsequent workflow builds on a consistent foundation
**Depends on**: Nothing (first phase)
**Requirements**: ARCH-01, ARCH-02, ARCH-03, ARCH-04
**Success Criteria** (what must be TRUE):
  1. An ADR defines the hierarchy: roadmap milestones are narrative (memory note), task board uses domain-structured epics/features/tasks, sequencing via blockers. "Milestone" ≠ Djinn execution phase.
  2. An ADR establishes the state derivation principle -- progress comes from live task board queries, never stored state notes
  3. A reference document maps every GSD artifact to a specific Djinn memory type and MCP tool call
  4. PROJECT.md contains no contradictory hierarchy references (the "Phases -> Epics not Features" entry is resolved)
**Plans**: 2 plans
- [x] 00-01-PLAN.md -- Create ADR-001 (Hierarchy Mapping) and ADR-002 (State Derivation) in Djinn memory
- [x] 00-02-PLAN.md -- Create Artifact Mapping reference note and update PROJECT.md contradictions

### Phase 1: Skill Scaffolding
**Goal**: The skill directory structure and shared patterns exist so workflow authors can focus on methodology, not plumbing
**Depends on**: Phase 0
**Requirements**: SKAF-01, SKAF-02, SKAF-03, SKAF-04, SKAF-05, SKAF-06
**Success Criteria** (what must be TRUE):
  1. SKILL.md files exist for all four v1 workflows (new-project, plan-phase, discuss-phase, progress) following the Agent Skills spec
  2. Each SKILL.md declares a focused tool subset (5-10 MCP tools) instead of exposing the full Djinn API
  3. Shared MCP adapter patterns (memory output templates, task creation templates) exist as reusable reference files
  4. Every workflow prompt fits within the 600-line budget
  5. The directory structure matches Claude Code plugin format and plugin.json is updated
**Plans**: TBD

### Phase 2: Core Workflow -- new-project
**Goal**: A user can run `/djinn:new-project` and get from zero to a populated Djinn memory (brief, research, requirements, roadmap) and task board (milestone epic, phase features) through guided questioning and parallel research
**Depends on**: Phase 1
**Requirements**: PROJ-01, PROJ-02, PROJ-03, PROJ-04, PROJ-05, PROJ-06, PROJ-07, PROJ-08, PROJ-09, PROJ-10, PROJ-11
**Success Criteria** (what must be TRUE):
  1. Running `/djinn:new-project` triggers a deep questioning session that follows threads and adapts (not a static checklist)
  2. After questioning, 4 parallel research agents produce dimension-specific research notes in Djinn memory (type=research), connected via wikilinks
  3. A synthesizer reads all research and produces a summary note in Djinn memory
  4. Requirements are generated with REQ-IDs and category grouping, written to Djinn memory (type=requirement)
  5. A roadmap note (type=roadmap) is created with milestones and success criteria, AND domain-structured Djinn epics/features are created with sequencing via blocker dependencies
**Plans**: TBD

### Phase 3: Core Workflow -- plan-milestone
**Goal**: A user can run `/djinn:plan-milestone {N}` and get a fully decomposed set of domain-structured Djinn tasks with acceptance criteria, design context, and wave-based ordering
**Depends on**: Phase 2
**Requirements**: PLAN-01, PLAN-02, PLAN-03, PLAN-04, PLAN-05, PLAN-06, PLAN-07, PLAN-08
**Success Criteria** (what must be TRUE):
  1. Running `/djinn:plan-milestone 1` reads the roadmap, requirements, and research from Djinn memory (not filesystem) and produces domain-structured epics/features/tasks
  2. Each created task has structured fields: acceptance_criteria, design context, and wave assignment
  3. Wave ordering is enforced via blocker dependencies -- wave 2 tasks are blocked by wave 1 completion
  4. A plan-checker validates that the task decomposition achieves the milestone's success criteria, with up to 3 revision iterations
  5. Tasks and memory notes are bidirectionally linked -- tasks reference memory note permalinks, memory notes reference task IDs
**Plans**: TBD

### Phase 4: Supporting Workflows
**Goal**: Users can enrich planning quality by discussing milestone context before planning, capturing design decisions and scope boundaries
**Depends on**: Phase 3
**Requirements**: SUPP-01, SUPP-02, SUPP-03
**Success Criteria** (what must be TRUE):
  1. Running `/djinn:discuss-milestone {N}` starts an adaptive questioning session that explores gray areas and scope boundaries for that milestone
  2. Design decisions made during discussion are captured as type=adr memory notes in Djinn
  3. Scope boundaries and preferences from discussion are stored in memory and consumed by plan-milestone when it runs
**Plans**: TBD

### Phase 5: Distribution
**Goal**: The Djinn planning system is installable by users on all four target runtimes (Claude Code, OpenCode, Gemini, Codex)
**Depends on**: Phase 4
**Requirements**: DIST-01, DIST-02, DIST-03, DIST-04, DIST-05
**Success Criteria** (what must be TRUE):
  1. Claude Code users get planning skills automatically via the existing plugin (plugin/skills/ directory extended)
  2. Non-Claude users can install via NPM and the installer configures MCP server connection for their runtime
  3. The Agent Skills spec (SKILL.md) is the universal format -- no per-runtime content transformation is needed
  4. Plugin hooks verify djinn-server is running before workflows execute, with a clear error message if not
**Plans**: TBD

## Progress

**Execution Order:**
Phases execute in numeric order: 0 -> 1 -> 2 -> 3 -> 4 -> 5

| Phase | Plans Complete | Status | Completed |
|-------|----------------|--------|-----------|
| 0. Architecture Decisions | 2/2 | Complete | 2026-03-02 |
| 1. Skill Scaffolding | 4/4 | Complete | 2026-03-02 |
| 2. Core Workflow -- new-project | 0/TBD | Not started | - |
| 3. Core Workflow -- plan-milestone | 0/TBD | Not started | - |
| 4. Supporting Workflows | 0/TBD | Not started | - |
| 5. Distribution | 0/TBD | Not started | - |
