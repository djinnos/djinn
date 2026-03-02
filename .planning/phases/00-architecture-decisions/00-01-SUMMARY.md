---
phase: 00-architecture-decisions
plan: 01
subsystem: architecture
tags: [adr, hierarchy-mapping, state-derivation, djinn-memory, wikilinks]

# Dependency graph
requires:
  - phase: none
    provides: "First plan in first phase -- no prior dependencies"
provides:
  - "ADR-001: Hierarchy Mapping -- defines epic/feature/task hierarchy and milestone narrative-only principle"
  - "ADR-002: State Derivation -- establishes live query pattern, immutable roadmap principle"
  - "Cross-referenced wikilink graph between both ADRs"
affects: [00-02-PLAN, 01-skill-scaffolding, 02-new-project, 03-plan-milestone]

# Tech tracking
tech-stack:
  added: []
  patterns: [djinn-memory-adr-format, wikilink-cross-referencing, disambiguation-tables]

key-files:
  created:
    - ".djinn/memory/decisions/adr-001-hierarchy-mapping.md"
    - ".djinn/memory/decisions/adr-002-state-derivation.md"
  modified: []

key-decisions:
  - "Milestones are narrative-only in roadmap note, NOT task board entities"
  - "Task board uses domain-structured epics (Auth System, not Milestone 1)"
  - "All progress derived from live task board queries -- no stored state"
  - "Roadmap memory note is immutable -- scope changes produce new version"

patterns-established:
  - "ADR lightweight format: Context/Decision/Consequences with Relations section, 50-100 lines"
  - "Wikilink cross-referencing: all Djinn memory notes link to related notes via [[Title]]"
  - "Disambiguation table pattern: clarify terms that sound similar but differ"

requirements-completed: [ARCH-01, ARCH-02]

# Metrics
duration: 3min
completed: 2026-03-02
---

# Phase 0 Plan 01: Architecture Decision Records Summary

**Two ADRs in Djinn memory resolving hierarchy mapping (milestones=narrative, epics=domain-structured) and state derivation (live queries, immutable roadmap)**

## Performance

- **Duration:** 3 min
- **Started:** 2026-03-02T11:58:24Z
- **Completed:** 2026-03-02T12:01:44Z
- **Tasks:** 2
- **Files modified:** 2

## Accomplishments

- ADR-001 defines the complete hierarchy: milestones are narrative goals in the roadmap note, epics are domain-structured, features are deliverables, tasks are implementation steps
- ADR-001 includes a disambiguation table making "milestone" vs "Djinn execution phase" crystal clear
- ADR-002 establishes that all progress comes from live task board queries with a pseudocode query chain showing exact MCP calls
- ADR-002 declares the roadmap memory note immutable -- workflows must never call memory_edit on it
- Both ADRs cross-reference each other and forward-link to [[Artifact Mapping]] and [[Roadmap]] (resolved by Plan 02 and Phase 2)

## Task Commits

Each task was committed atomically:

1. **Task 1: Create ADR-001 Hierarchy Mapping in Djinn Memory** - `8a23ddc` (feat)
2. **Task 2: Create ADR-002 State Derivation in Djinn Memory** - `54b5caf` (feat)

## Files Created/Modified

- `.djinn/memory/decisions/adr-001-hierarchy-mapping.md` - Hierarchy mapping ADR (68 lines): milestone/epic/feature/task definitions, disambiguation table
- `.djinn/memory/decisions/adr-002-state-derivation.md` - State derivation ADR (67 lines): live query principle, pseudocode query chain, immutable roadmap

## Decisions Made

- Both ADRs stored directly in Djinn memory filesystem (`.djinn/memory/decisions/`) rather than via MCP tool calls, since MCP tools are not available in this CLI execution context. The ADR content follows the exact same format and will be indexed by Djinn on next server startup.
- Used YAML frontmatter with title, type, and tags fields to match Djinn's expected note format for proper indexing.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Direct filesystem write instead of MCP memory_write**
- **Found during:** Task 1 (ADR-001 creation)
- **Issue:** Plan specified using `memory_write()` MCP tool calls, but Djinn MCP tools are not available in this CLI agent context
- **Fix:** Created ADR files directly at `.djinn/memory/decisions/` matching Djinn's filesystem layout with proper YAML frontmatter (title, type, tags)
- **Files modified:** `.djinn/memory/decisions/adr-001-hierarchy-mapping.md`, `.djinn/memory/decisions/adr-002-state-derivation.md`
- **Verification:** Files exist with correct content, proper frontmatter, all required wikilinks
- **Committed in:** `8a23ddc`, `54b5caf`

---

**Total deviations:** 1 auto-fixed (1 blocking)
**Impact on plan:** Minimal. ADR content is identical to what memory_write would produce. Djinn will index these files on next startup. No content compromise.

## Issues Encountered

- `.djinn/` and `.planning` directories are both gitignored. Used `git add -f` to force-add ADR files to git, matching the pattern established by previous commits (e.g., `82bf738` which force-added `.planning/research/` files).
- Djinn memory catalog (`catalog.md`) was empty -- no existing notes to conflict with. Fresh memory state confirmed.

## User Setup Required

None - no external service configuration required.

## Next Phase Readiness

- Both foundational ADRs are in place for Plan 02 (Artifact Mapping reference note and PROJECT.md updates)
- [[Artifact Mapping]] and [[Roadmap]] wikilinks are known forward-references that will be resolved by Plan 02 and Phase 2 respectively
- Plan 02 can reference ADR-001 and ADR-002 directly via wikilinks

## Self-Check: PASSED

All files exist. All commits verified.

---
*Phase: 00-architecture-decisions*
*Completed: 2026-03-02*
