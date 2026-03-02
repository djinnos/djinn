---
phase: 04-supporting-workflows
plan: 01
subsystem: workflow
tags: [discuss-milestone, adr, scope-note, adaptive-discussion, djinn-memory]

# Dependency graph
requires:
  - phase: 01-skill-scaffolding
    provides: discuss-milestone SKILL.md scaffold with 6-step structure and extension point markers
  - phase: 03-plan-milestone
    provides: plan-milestone SKILL.md that consumes scope notes and ADRs
provides:
  - Complete discuss-milestone workflow with adaptive discussion methodology
  - ADR batch write logic with granularity filter and cross-referencing
  - Scope note validation with completeness checks and wikilink aggregation
affects: [05-distribution]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "Decision-driven checkpoints (confirm when decision crystallizes, not fixed question count)"
    - "Granularity filter for ADR vs preference classification"
    - "Scope note as single entry point with wikilink aggregation to all session ADRs"

key-files:
  created: []
  modified:
    - plugin/skills/djinn-planning/discuss-milestone/SKILL.md

key-decisions:
  - "Step 3 uses heuristics and principles, not rigid dialog scripts, enabling organic conversation flow"
  - "ADR granularity test: 'Would a different choice here change how tasks are structured or what code gets written?'"
  - "Scope note existing-session handling uses memory_edit replace_section, not overwrite"

patterns-established:
  - "Context presentation before topic identification: show milestone goal, requirements, research themes, existing ADRs"
  - "Decision-driven checkpoint pattern: confirm when decision crystallizes, ask 'Anything else or move on?'"
  - "Scope creep redirection: note deferred idea for Out of Scope, redirect to current topic"
  - "Completeness check: every discussed topic must appear in ADR, In Scope, Out of Scope, or Preferences"

requirements-completed: [SUPP-01, SUPP-02, SUPP-03]

# Metrics
duration: 2min
completed: 2026-03-02
---

# Phase 4 Plan 01: Discuss-Milestone Extension Points Summary

**Adaptive discussion methodology with decision-driven checkpoints, batch ADR write with granularity filtering, and scope note validation with completeness checks**

## Performance

- **Duration:** 2 min
- **Started:** 2026-03-02T14:07:29Z
- **Completed:** 2026-03-02T14:09:40Z
- **Tasks:** 2
- **Files modified:** 1

## Accomplishments
- Filled all three Phase 4 extension points in discuss-milestone SKILL.md, transforming it from scaffold to complete workflow
- Step 3: Adaptive discussion methodology with context presentation, decision-driven checkpoints, thread-following heuristics, scope creep redirection, and topic completion flow
- Step 4: Batch ADR write logic with granularity filter (ADR-worthy vs preference-only), numbering discovery, cross-referencing, and quality checks
- Step 5: Scope note validation with section assembly, wikilink aggregation to all session ADRs, completeness check, and existing scope note handling

## Task Commits

Each task was committed atomically:

1. **Task 1: Fill Step 3 (adaptive discussion methodology)** - `fa3ad09` (feat)
2. **Task 2: Fill Step 4 (ADR capture) and Step 5 (scope note)** - `852a512` (feat)

## Files Created/Modified
- `plugin/skills/djinn-planning/discuss-milestone/SKILL.md` - Complete discuss-milestone workflow with all extension points filled (307 lines, within 600-line budget)

## Decisions Made
- Step 3 written as heuristics and principles ("when you notice X, do Y") rather than rigid dialog scripts, enabling organic conversation
- ADR granularity threshold codified as a simple test: "Would a different choice change how tasks are structured or what code gets written?"
- Existing scope note handling uses `memory_edit` with `replace_section` operation to update individual sections, preserving the append-only wikilink pattern for Relations

## Deviations from Plan

None - plan executed exactly as written.

## Issues Encountered
None

## User Setup Required
None - no external service configuration required.

## Next Phase Readiness
- Phase 4 is complete (single plan). All discuss-milestone extension points are filled.
- The complete SKILL.md is ready for distribution in Phase 5.
- plan-milestone can now consume scope notes and ADRs produced by discuss-milestone.

## Self-Check: PASSED

- FOUND: plugin/skills/djinn-planning/discuss-milestone/SKILL.md
- FOUND: .planning/phases/04-supporting-workflows/04-01-SUMMARY.md
- FOUND: fa3ad09 (Task 1 commit)
- FOUND: 852a512 (Task 2 commit)

---
*Phase: 04-supporting-workflows*
*Completed: 2026-03-02*
