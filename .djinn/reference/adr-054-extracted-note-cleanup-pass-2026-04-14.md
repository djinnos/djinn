# ADR-054 extracted-note cleanup pass — 2026-04-14

Originated from task 019d89c3-6137-7a13-85d3-f16633150f55.

## Scope

This cleanup pass stays focused on the pre-existing extracted `case` / `pattern` / `pitfall` backlog called out by ADR-054. It does **not** change extraction policy again; it provides a reproducible manifest-driven pass plus concrete corpus edits for a narrow, high-confidence slice.

## Reproducible procedure

From the repo root:

```bash
python server/scripts/adr054_extracted_note_cleanup.py \
  --db ~/.djinn/djinn.db \
  --project /home/fernando/git/djinnos/djinn \
  --dry-run
```

To apply the same manifest against a writable maintenance database:

```bash
python server/scripts/adr054_extracted_note_cleanup.py \
  --db /path/to/writable/djinn.db \
  --project /home/fernando/git/djinnos/djinn \
  --apply
```

If the selected database is mounted read-only, the script exits non-zero and emits the same before/projected-after evidence plus a writable-database hint.

## Cleanup categories reconciled in this pass

### Merge duplicate families into one strengthened durable note

- `cases/adr-049-skill-discovery-implemented-only-in-djinn-agent-seam`
  - strengthened into ADR-054 case template shape
  - duplicate siblings marked archived/superseded:
    - `cases/adr-049-skill-discovery-limited-to-djinn-agent-seam`
    - `cases/adr-049-skill-discovery-updated-in-djinn-agent-seam-only`
- `cases/merged-semantic-retrieval-into-note-memory-search-without-changing-the-mcp-interface`
  - strengthened into ADR-054 case template shape
  - duplicate siblings marked archived/superseded:
    - `cases/blend-semantic-retrieval-into-existing-note-search-without-changing-the-mcp-interface`
    - `cases/added-vector-aware-ranking-to-the-existing-note-search-pipeline`
    - `cases/blend-semantic-vector-search-into-existing-memory-search-ranking`
    - `cases/thread-semantic-search-context-through-bridge-and-state-layers`

### Rewrite underspecified durable notes

- `pitfalls/some-planner-dispatches-omit-memory-write-edit-tools`
  - rewritten from one extracted paragraph into the ADR-054 pitfall template

### Demote task-local notes into Working Specs

Created file-backed design notes:

- `design/working-spec-adr-055-sqlite-seam-inventory`
- `design/working-spec-adr-055-task-knowledge-branching-rollout`

Their originating extracted case notes were annotated as superseded working-context sources:

- `cases/adr-roadmap-captured-sqlite-migration-seam-inventory-categories`
- `cases/adr-055-integration-contract-for-per-task-knowledge-branching`

### Archive low-value leftovers

The manifest identifies six duplicate extracted leftovers for archival once run against a writable DB. In the current environment they were additionally annotated as archived/superseded provenance residue so the intended archive set is explicit even before destructive deletion is possible.

## Measured evidence

### Dry-run rerun captured on 2026-04-14 in the review environment

```json
{
  "before": {
    "heuristic_audit": {
      "archive_candidates_rough": 824,
      "demote_rough": 21,
      "scanned_note_count": 838,
      "underspecified_rough": 835
    },
    "targeted_status": {
      "demoted_working_spec_rows": 0,
      "planner_tool_pitfall_present": true,
      "remaining_archive_targets": 6,
      "semantic_retrieval_family_rows": 5,
      "skill_discovery_family_rows": 3
    }
  },
  "mode": "dry_run",
  "project_id": "019d2f47-d38f-7750-a333-ec60dee8661c",
  "projected_after": {
    "heuristic_audit": {
      "archive_candidates_rough": 824,
      "demote_rough": 21,
      "scanned_note_count": 830,
      "underspecified_rough": 827
    },
    "targeted_status": {
      "demoted_working_spec_rows": 2,
      "planner_tool_pitfall_present": true,
      "remaining_archive_targets": 0,
      "semantic_retrieval_family_rows": 1,
      "skill_discovery_family_rows": 1
    }
  }
}
```

### Interpretation

- The rerun confirms the remaining ADR-054 backlog is still large (`838` scanned extracted notes), which keeps the residual migration scope measurable.
- This pass intentionally targets a narrow, high-confidence slice.
- Within that slice, the manifest reduces:
  - skill-discovery duplicate family size from `3` notes to `1`
  - semantic-retrieval duplicate family size from `5` notes to `1`
  - pending archive leftovers from `6` to `0`
  - demoted Working Spec targets from `0` to `2`
- The projected `scanned_note_count` drop from `838` to `830` reflects six archive deletions plus two demotions out of the extracted taxonomy.

## Writable-database caveat captured during verification

Attempting `--apply` against `~/.djinn/djinn.db` in this session returned:

```json
{
  "error": "attempt to write a readonly database",
  "hint": "The selected database is not writable. Use --dry-run for measurement, or run --apply against a writable database copy / maintenance instance."
}
```

That readonly restriction affected the manifest-driven destructive DB pass only. File-backed Working Spec notes and direct memory-note rewrites were still applied during this task where the available tool surfaces allowed them.
