---
title: ADR-054 Roadmap — Memory Extraction Quality Gates and Note Taxonomy
type: design
tags: ["adr-054","roadmap","memory","extraction","quality-gates"]
---

# ADR-054 Roadmap — Memory Extraction Quality Gates and Note Taxonomy

## Status
In progress.

Most ADR-054 implementation work is landed: extraction quality-gate decisions, structured templates, Working Spec routing, access-tracking for MCP search/retrieval flows, and roadmap/design link cleanup are complete. The epic is now in its closure wave.

## Completed waves

### Wave 1 — extraction policy and write-time quality gates
- `xp8p` implemented richer extraction outcomes and quality-gate decisions in `llm_extraction.rs`
- `gl52` enforced structured templates for durable `pattern` / `pitfall` / `case` notes
- `2x4r` introduced Working Spec routing for non-durable extracted knowledge

### Wave 2 — rollout and audit tooling
- `gter` added a repeatable audit path for extracted-note taxonomy/template violations
- `wue6` completed access-tracking coverage for MCP search/retrieval flows
- `pd8e` repaired ADR-054-related design/roadmap broken links with canonical targets

## Active closure work
- `8vh1` is in verification for the corpus-cleanup pass that reconciles extracted notes across merge, rewrite, demote-to-working-spec, and archive categories.

## Closure blockers
1. Verify `8vh1` landed the intended cleanup artifacts and rerun evidence.
2. Reconcile missing canonical memory references currently cited by the epic/task board:
   - `design/adr-054-roadmap-memory-extraction-quality-gates-and-note-taxonomy`
   - `design/working-spec-adr-055-sqlite-seam-inventory`
   - `design/working-spec-adr-055-task-knowledge-branching-rollout`
3. Once cleanup verification passes and memory refs resolve canonically, close epic `3ch7` without creating further implementation work.

## Next planner action
If the verification of `8vh1` confirms the cleanup and the missing memory refs are resolved, the next planning pass should close the epic immediately. If the refs are still missing, route only the minimal reconciliation work needed to materialize or relink those canonical notes.

## Relations
- [[decisions/adr-054-proposal-memory-artifact-hygiene-and-proactive-knowledge-curation]]
- [[decisions/adr-055-proposal-dolt-migration-and-per-task-knowledge-branching]]
- [[cases/adr-049-skill-discovery-implemented-only-in-djinn-agent-seam]]
- [[cases/merged-semantic-retrieval-into-note-memory-search-without-changing-the-mcp-interface]]
- [[pitfalls/some-planner-dispatches-omit-memory-write-edit-tools]]
