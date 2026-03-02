# Requirements: Djinn Planning System

**Defined:** 2026-03-02
**Core Value:** GSD's planning methodology outputs directly into Djinn's memory and task systems, eliminating the bridge between planning and execution.

## v1 Requirements

Requirements for initial release. Each maps to roadmap phases.

### Architecture Foundation

- [x] **ARCH-01**: Hierarchy mapping resolved as ADR — roadmap milestones are narrative (memory note), task board uses domain-structured epics/features/tasks, sequencing via blockers
- [x] **ARCH-02**: State derivation principle established — all progress derived from live task board queries, no stored state notes
- [x] **ARCH-03**: Artifact mapping reference note created — maps every GSD artifact to a Djinn memory type and MCP call
- [x] **ARCH-04**: PROJECT.md contradictions resolved (phases as epics vs features)

### Skill Scaffolding

- [x] **SKAF-01**: SKILL.md files created for all v1 workflows following Agent Skills spec
- [x] **SKAF-02**: Directory structure matches Claude Code plugin format (skills/djinn-planning/)
- [x] **SKAF-03**: Each workflow declares a tool subset (5-10 MCP tools, not 70+) to prevent context explosion
- [x] **SKAF-04**: Progressive disclosure pattern — metadata → instructions → references/cookbooks
- [x] **SKAF-05**: Shared MCP adapter patterns extracted as reusable references (memory output templates, task creation templates)
- [x] **SKAF-06**: Workflow prompt budgets enforced (max 600 lines per workflow)

### Core Planning: new-project

- [ ] **PROJ-01**: `/djinn:new-project` command triggers full project initialization flow
- [ ] **PROJ-02**: Deep questioning methodology preserved verbatim from GSD (thread-following, not checklist)
- [ ] **PROJ-03**: Project brief written to Djinn memory as type=brief via memory_write
- [ ] **PROJ-04**: 4 parallel research agents spawn (stack, features, architecture, pitfalls), each writing to Djinn memory as type=research
- [ ] **PROJ-05**: Research synthesizer reads all 4 research notes and writes summary to Djinn memory
- [ ] **PROJ-06**: Requirements definition with REQ-IDs, category grouping, v1/v2/out-of-scope classification
- [ ] **PROJ-07**: Requirements written to Djinn memory as type=requirement
- [ ] **PROJ-08**: Roadmap generation creates narrative roadmap note (type=roadmap) with phases, success criteria, requirement traceability
- [ ] **PROJ-09**: Roadmap informs creation of domain-structured Djinn epics and features (e.g., "Auth System" not "Milestone 1"), with sequencing via blocker dependencies
- [ ] **PROJ-10**: Research notes use wikilinks to connect findings across dimensions
- [ ] **PROJ-11**: Workflow configuration preferences collected (depth, parallelization, model profile) and stored as type=reference

### Core Planning: plan-milestone

- [ ] **PLAN-01**: `/djinn:plan-milestone {N}` command triggers phase planning for a specific phase
- [ ] **PLAN-02**: Phase researcher agent investigates domain context before planning, writes to Djinn memory as type=research
- [ ] **PLAN-03**: Planner agent decomposes phase into tasks with acceptance_criteria, design fields, and wave assignments
- [ ] **PLAN-04**: Tasks created in Djinn via task_create under the phase's feature, with structured fields
- [ ] **PLAN-05**: Wave ordering enforced via blocker dependencies (wave 2 tasks blocked by wave 1 tasks)
- [ ] **PLAN-06**: Plan-checker agent validates plan achieves phase goals, up to 3 revision iterations
- [ ] **PLAN-07**: Plan reads roadmap, requirements, and research from Djinn memory (not filesystem)
- [ ] **PLAN-08**: Bidirectional memory-task linking — tasks reference memory note permalinks, memory notes reference task IDs

### Supporting Workflows

- [ ] **SUPP-01**: `/djinn:discuss-milestone {N}` command gathers phase context through adaptive questioning
- [ ] **SUPP-02**: Design decisions captured as type=adr memory notes during discussion
- [ ] **SUPP-03**: Scope boundaries and preferences stored in memory for plan-milestone to consume

### Distribution

- [ ] **DIST-01**: Claude Code plugin extended with planning skills in plugin/skills/ directory
- [ ] **DIST-02**: NPM package with multi-runtime installer (OpenCode, Gemini, Codex)
- [ ] **DIST-03**: Installer configures MCP server connection per runtime
- [ ] **DIST-04**: Agent Skills spec (SKILL.md) used as universal format — no per-runtime content transformation
- [ ] **DIST-05**: Plugin hooks ensure djinn-server daemon is running before workflows execute

## v2 Requirements

Deferred to future release. Tracked but not in current roadmap.

### Progress & Routing

- **PROG-01**: `/djinn:progress` command queries task board and routes to next action
- **PROG-02**: Progress derives state from task_list and execution_phase_list, not stored state

### Milestone Management

- **MILE-01**: `/djinn:complete-milestone` archives completed work
- **MILE-02**: `/djinn:new-milestone` starts next iteration with updated requirements
- **MILE-03**: `/djinn:audit-milestone` validates milestone against original intent

### Administrative Workflows

- **ADMN-01**: `/djinn:add-phase` adds phase to roadmap
- **ADMN-02**: `/djinn:remove-phase` removes future phase
- **ADMN-03**: `/djinn:settings` configures workflow preferences
- **ADMN-04**: `/djinn:health` diagnoses project health

### Advanced Features

- **ADVN-01**: Quick task mode for ad-hoc work outside phase structure
- **ADVN-02**: Brownfield codebase mapping integration
- **ADVN-03**: Cross-project knowledge reuse via Djinn memory graph
- **ADVN-04**: Pause/resume workflows with context handoff

## Out of Scope

| Feature | Reason |
|---------|--------|
| Execution engine (execute-phase, execute-plan) | Djinn orchestrator owns execution — not replicated |
| Verification workflows (verify-work, verify-phase) | Djinn review pipeline handles post-execution quality |
| GSD state management tooling (gsd-tools.cjs, STATE.md) | Replaced by Djinn task board queries |
| .planning/ filesystem storage | Replaced entirely by Djinn memory MCP |
| Debug workflow | Djinn has its own debugging patterns |
| Todo tracking (add-todo, check-todos) | Djinn tasks replace this |
| Test planning (add-tests) | Deferred — Djinn tasks can carry test requirements |

## Traceability

Which phases cover which requirements. Updated during roadmap creation.

| Requirement | Phase | Status |
|-------------|-------|--------|
| ARCH-01 | Phase 0 | Complete |
| ARCH-02 | Phase 0 | Complete |
| ARCH-03 | Phase 0 | Complete |
| ARCH-04 | Phase 0 | Complete |
| SKAF-01 | Phase 1 | Complete |
| SKAF-02 | Phase 1 | Complete |
| SKAF-03 | Phase 1 | Complete |
| SKAF-04 | Phase 1 | Complete |
| SKAF-05 | Phase 1 | Complete |
| SKAF-06 | Phase 1 | Complete |
| PROJ-01 | Phase 2 | Pending |
| PROJ-02 | Phase 2 | Pending |
| PROJ-03 | Phase 2 | Pending |
| PROJ-04 | Phase 2 | Pending |
| PROJ-05 | Phase 2 | Pending |
| PROJ-06 | Phase 2 | Pending |
| PROJ-07 | Phase 2 | Pending |
| PROJ-08 | Phase 2 | Pending |
| PROJ-09 | Phase 2 | Pending |
| PROJ-10 | Phase 2 | Pending |
| PROJ-11 | Phase 2 | Pending |
| PLAN-01 | Phase 3 | Pending |
| PLAN-02 | Phase 3 | Pending |
| PLAN-03 | Phase 3 | Pending |
| PLAN-04 | Phase 3 | Pending |
| PLAN-05 | Phase 3 | Pending |
| PLAN-06 | Phase 3 | Pending |
| PLAN-07 | Phase 3 | Pending |
| PLAN-08 | Phase 3 | Pending |
| SUPP-01 | Phase 4 | Pending |
| SUPP-02 | Phase 4 | Pending |
| SUPP-03 | Phase 4 | Pending |
| DIST-01 | Phase 5 | Pending |
| DIST-02 | Phase 5 | Pending |
| DIST-03 | Phase 5 | Pending |
| DIST-04 | Phase 5 | Pending |
| DIST-05 | Phase 5 | Pending |

**Coverage:**
- v1 requirements: 33 total
- Mapped to phases: 33
- Unmapped: 0 ✓

---
*Requirements defined: 2026-03-02*
*Last updated: 2026-03-02 after initial definition*
