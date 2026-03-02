---
phase: 01-skill-scaffolding
plan: 03
subsystem: planning-workflows
tags: [agent-skills, skill-scaffolding, mcp-tools, workflow-design]

# Dependency graph
requires:
  - phase: 01-skill-scaffolding/01-01
    provides: "Router SKILL.md, directory structure, plugin.json registration"
  - phase: 01-skill-scaffolding/01-02
    provides: "Shared cookbooks (planning-templates.md, task-templates.md)"
provides:
  - "new-project sub-workflow SKILL.md with 9-step scaffold"
  - "plan-milestone sub-workflow SKILL.md with 8-step scaffold"
affects: [02-new-project-workflow, 03-plan-milestone-workflow]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "Sub-workflow SKILL.md with no Agent Skills frontmatter (plain markdown title)"
    - "Tools table with name + one-line purpose (5-10 tools per workflow)"
    - "Category-based Do NOT Use section grouping excluded tools by concern"
    - "Phase extension point markers ([Phase N implements ... here])"
    - "Cookbook references via See cookbook/*.md pattern"

key-files:
  created:
    - "plugin/skills/djinn-planning/new-project/SKILL.md"
    - "plugin/skills/djinn-planning/plan-milestone/SKILL.md"
  modified: []

key-decisions:
  - "new-project uses 7 tools focused on memory creation and task board setup"
  - "plan-milestone uses 8 tools focused on memory reading and task decomposition"
  - "plan-milestone excludes memory_write from allowed tools (reads context, doesn't create planning artifacts)"
  - "Both files include Reference appendix section (Layer 3) with Phase extension point tables"

patterns-established:
  - "Sub-workflow structure: Title -> Goal -> Tools -> Do NOT Use -> Workflow Steps -> Output Summary -> Reference appendix"
  - "Extension points marked with [Phase N implements ... here] pattern for clear Phase 2/3 handoff"

requirements-completed: [SKAF-01, SKAF-03, SKAF-06]

# Metrics
duration: 3min
completed: 2026-03-02
---

# Phase 01 Plan 03: Sub-Workflow SKILL.md Scaffolding Summary

**new-project (197 lines, 9 steps, 7 tools) and plan-milestone (192 lines, 8 steps, 8 tools) sub-workflow SKILL.md files with complete structural scaffolding ready for Phase 2/3 methodology**

## Performance

- **Duration:** 3 min
- **Started:** 2026-03-02T13:03:35Z
- **Completed:** 2026-03-02T13:07:00Z
- **Tasks:** 2
- **Files created:** 2

## Accomplishments
- new-project SKILL.md scaffolded with 9-step workflow covering orient, questioning, brief, research, synthesis, requirements, roadmap, task board setup, and verification
- plan-milestone SKILL.md scaffolded with 8-step workflow covering context loading, domain research, epic identification, task decomposition, wave ordering, validation, bidirectional linking, and output summary
- Both files follow the sub-workflow pattern: no Agent Skills frontmatter, plain markdown title, tools table, Do NOT Use section, numbered workflow steps, cookbook references
- Phase 2 and Phase 3 extension points clearly marked for subsequent implementation

## Task Commits

Each task was committed atomically:

1. **Task 1: Create new-project sub-workflow SKILL.md** - `5f88d88` (feat)
2. **Task 2: Create plan-milestone sub-workflow SKILL.md** - `2a9e16e` (feat)

## Files Created/Modified
- `plugin/skills/djinn-planning/new-project/SKILL.md` - New project initialization workflow scaffold (197 lines)
- `plugin/skills/djinn-planning/plan-milestone/SKILL.md` - Milestone planning workflow scaffold (192 lines)

## Decisions Made
- **new-project tool set (7 tools)**: memory_write, memory_read, memory_search, memory_catalog for knowledge creation; task_create, task_blockers_add, task_update for task board setup. Matches the research recommendation exactly.
- **plan-milestone tool set (8 tools)**: memory_read, memory_search, memory_build_context for context loading; task_create, task_update, task_blockers_add, task_list, task_children_list for task decomposition. Matches the research recommendation exactly.
- **plan-milestone excludes memory_write**: This workflow reads existing planning artifacts from memory; it does not create new ones (brief, research, requirements, roadmap are created by new-project). Exception noted for optional researcher agent in Step 2.
- **Layer 3 appendix kept minimal**: Both files include a Reference section with a Phase extension point table, but no detailed methodology content (that is Phase 2/3 scope).

## Deviations from Plan

None - plan executed exactly as written.

## Issues Encountered

None

## User Setup Required

None - no external service configuration required.

## Next Phase Readiness
- new-project/SKILL.md ready for Phase 2 to implement questioning methodology and parallel research agent pattern
- plan-milestone/SKILL.md ready for Phase 3 to implement context loading, researcher agent, and plan-checker
- Both files provide clear extension points marked with `[Phase N implements ... here]`
- Remaining Phase 1 work: Plan 01-04 (discuss-milestone and progress stub SKILL.md files)

---
*Phase: 01-skill-scaffolding*
*Completed: 2026-03-02*

## Self-Check: PASSED
