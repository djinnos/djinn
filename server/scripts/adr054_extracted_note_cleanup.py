#!/usr/bin/env python3
"""Targeted ADR-054 cleanup pass for extracted case/pattern/pitfall notes.

This script applies a reproducible, manifest-driven cleanup against the live
Djinn SQLite database. It intentionally focuses on a small set of pre-audited
families so the cleanup wave is repeatable and measurable without changing the
underlying extraction policy again.

Actions performed:
- merge duplicate extracted case families into one strengthened canonical note
- rewrite underspecified durable notes to ADR-054 template shape
- demote task-local extracted notes into db-backed design/Working Spec notes
- archive low-value extracted leftovers by deleting them

Usage:
  python server/scripts/adr054_extracted_note_cleanup.py --db ~/.djinn/djinn.db --project /home/fernando/git/djinnos/djinn --dry-run
  python server/scripts/adr054_extracted_note_cleanup.py --db ~/.djinn/djinn.db --project /home/fernando/git/djinnos/djinn --apply
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sqlite3
import sys
from dataclasses import dataclass
from typing import Iterable

PROJECT_PATH_DEFAULT = "/home/fernando/git/djinnos/djinn"
WIKILINK_RE = re.compile(r"\[\[([^\]|]+)(?:\|([^\]]+))?\]\]")
EXTRACTED_FOOTER = "*Extracted from session "


SKILL_DISCOVERY_CANONICAL = """## Situation
ADR-049 follow-up work updated `djinn-agent` skill discovery so requested skills no longer depended on a single legacy `.djinn/skills/*.md` lookup. The implementation had to support both older flat files and newer directory-based skill layouts without broadening the change into unrelated server or MCP seams.

## Constraint
Skill resolution still needed deterministic first-found-wins behavior by requested skill name, and existing frontmatter parsing plus optional `references/` content loading had to keep working. The task also needed to stay scoped to `server/crates/djinn-agent/src` so adjacent watcher and MCP bridge work did not get mixed into the rollout.

## Approach taken
The lookup path was expanded to search four standard locations in priority order: `.claude/skills/<name>/SKILL.md`, `.opencode/skills/<name>/SKILL.md`, `.djinn/skills/<name>.md`, and `.djinn/skills/<name>/SKILL.md`. The same skill-name request flow was kept, flat-file and directory-based frontmatter parsing stayed intact, readable `references/` content from directory skills was appended, and missing requested skills remained non-fatal.

## Result
Requested skills can now be resolved from the shared skill locations used by Claude, OpenCode, and Djinn while preserving compatibility with the earlier Djinn-only layout. The change stayed isolated to the djinn-agent seam and did not require broader server-side repair work.

## Why it worked / failed
It worked because the new search order added compatibility at the lookup boundary instead of rewriting downstream skill parsing or changing the caller contract. Keeping the search deterministic and first-found-wins avoided ambiguity while letting newer shared skill conventions coexist with the legacy path.

## Reusable lesson
When broadening a discovery mechanism to support multiple ecosystem conventions, extend lookup order at the seam that owns resolution and preserve the existing caller contract. That lets compatibility improve without forcing unrelated migrations in adjacent subsystems.

## Related
- [[decisions/adr-049-mcp-marketplace-and-skills-discovery]]
- [[patterns/constrain-changes-to-the-intended-seam]]
- [[pitfalls/avoid-broad-fixes-outside-the-targeted-seam]]

---
Consolidated ADR-054 cleanup pass on 2026-04-14 from duplicate extracted case variants that covered the same djinn-agent skill-discovery outcome.
"""

SEMANTIC_RETRIEVAL_CANONICAL = """## Situation
ADR-053 semantic-memory work needed to add embedding-backed retrieval to note search without breaking the existing `memory_search` MCP contract. Multiple session extracts captured the same implementation from slightly different angles: repository ranking changes, bridge/state plumbing, and fallback behavior.

## Constraint
Client-facing request and response shapes had to remain stable because existing MCP consumers already depended on the current memory-search contract. The integration also had to preserve degraded behavior when embeddings or vector lookup were unavailable, and the ranking changes needed to stay concentrated in the repository/search seam rather than leaking semantic-specific branching into every memory tool.

## Approach taken
The search path kept the existing MCP surface and repository entry point, then threaded internal semantic-search context through bridge/state wiring into the note repository. Query embeddings and vector-similarity results were blended into the established full-text and reciprocal-rank-fusion flow, while fallback behavior preserved the previous text-only path when semantic retrieval could not run.

## Result
Semantic retrieval now participates in `memory_search` as an additional retrieval signal rather than a separate client-visible mode. Repository tests and higher-level tool coverage were updated around ranking and degraded fallback behavior, and the MCP-facing interface remained unchanged for callers.

## Why it worked / failed
It worked because the semantic feature was introduced behind existing seams: internal plumbing carried new context, but the public contract did not change. That let repository ranking absorb the new signal while callers continued using the same search API and fallback semantics.

## Reusable lesson
When adding a new retrieval backend to an established search experience, preserve the external contract and integrate the new signal behind the repository boundary. Use internal-only plumbing for context propagation so adjacent bridge or tool layers do not become semantic-specific forks.

## Related
- [[decisions/adr-053-semantic-memory-search-candle-embeddings-with-sqlite-vec]]
- [[design/semantic-memory-search-candle-embeddings-with-sqlite-vec-roadmap]]
- [[patterns/add-new-retrieval-signals-behind-a-stable-search-interface]]
- [[pitfalls/breaking-search-consumers-by-changing-the-mcp-facing-contract]]

---
Consolidated ADR-054 cleanup pass on 2026-04-14 from duplicate extracted case variants covering the same semantic-memory search integration.
"""

PLANNER_TOOL_PITFALL = """## Trigger / smell
A planner or patrol session identifies exact canonical memory changes, but the dispatched tool bundle does not include `memory_write` or `memory_edit`.

## Failure mode
The session can diagnose the required cleanup but cannot execute it, so durable memory debt remains open even though the fix is already understood.

## Observable symptoms
- The session leaves precise note-edit instructions instead of applying them
- Follow-up work depends on handing the same memory changes to another role or rerunning with a richer tool bundle
- Cleanup tasks stall even though the missing action is mechanical rather than analytical

## Prevention
Validate planner/patrol tool bundles against the intended maintenance responsibilities before dispatch. If a role is expected to perform canonical note cleanup, ensure memory write and edit tools are present in the exported tool set.

## Recovery
Record the exact required memory edits, then reroute the work to a session context that includes canonical memory write capability. Treat the missing tools as the defect to repair rather than reopening analysis of the already-known note changes.

## Related
- [[decisions/adr-056-proposal-planner-driven-codebase-learning-and-memory-hygiene]]
- [[cases/add-a-recovery-path-for-mis-routed-proposal-drafts]]
"""

SQLITE_SEAM_WORKING_SPEC = """# Working Spec

## Active objective
- Track the ADR-055 SQLite migration seam inventory that was captured during roadmap/design work.
- Keep this as mutable planning context until the Dolt/MySQL migration wave decides which seams become durable canonical guidance.

## Relevant scope
- `.djinn/design`
- `server/src/db`
- `server/crates/djinn-db/src`

## Constraints
- This note is task-scoped working context promoted out of an extracted case because the original note only captured current migration inventory categories.
- The content is useful for ongoing ADR-055 implementation planning, but it is not yet a durable cross-task precedent.

## Current hypotheses
- Database bootstrap, migrations, lexical search, vector storage, and repository APIs are the highest-friction SQLite coupling buckets.
- The final durable notes should likely be a design or reference artifact rather than a historical case extract.

## Open questions
- Which seam inventory slices should become canonical design docs versus temporary migration checklists?
- When the ADR-055 rollout lands, should this note be promoted into a broader design/reference note or discarded?

## Captured session knowledge
The original extracted case observed that ADR-055 design work enumerated explicit SQLite-coupled surfaces so migration could proceed through known seams instead of scattered edits. The inventory was organized around bootstrap, migrations, lexical search, semantic vector storage, and repository APIs.

---
Demoted from extracted case during ADR-054 cleanup on 2026-04-14.
"""

BRANCHING_WORKING_SPEC = """# Working Spec

## Active objective
- Preserve the per-task knowledge-branching lifecycle captured during ADR-055 design work as mutable rollout context.
- Keep the wiring contract available while task dispatch, branch-scoped memory capture, and post-task promotion continue evolving.

## Relevant scope
- `.djinn/design`
- `server/crates/djinn-agent/src`
- `server/crates/djinn-db/src/repositories/note`

## Constraints
- The original extracted case described current architectural flow rather than a stable reusable precedent.
- This content should live as working context until the branching lifecycle settles into durable implementation guidance.

## Current hypotheses
- Task dispatch, session-memory writes, and promotion/cleanup hooks must remain an explicit lifecycle contract.
- Durable guidance should eventually live in canonical ADR/design material instead of an extracted historical case note.

## Open questions
- Which parts of the branching contract belong in enduring design docs versus temporary rollout coordination notes?
- What promotion and cleanup hooks still need to land before this can be rewritten as a durable case or pattern?

## Captured session knowledge
The original extracted case defined the concrete architectural flow for per-task knowledge branching so implementation could connect task dispatch, session-memory writes, and post-task promotion without re-deriving the lifecycle.

---
Demoted from extracted case during ADR-054 cleanup on 2026-04-14.
"""


@dataclass(frozen=True)
class UpdateAction:
    permalink: str
    title: str
    note_type: str
    folder: str
    content: str
    tags_json: str | None = None


@dataclass(frozen=True)
class ReclassifyAction:
    source_permalink: str
    title: str
    note_type: str
    folder: str
    permalink: str
    content: str
    tags_json: str


CANONICAL_UPDATES = [
    UpdateAction(
        permalink="cases/adr-049-skill-discovery-implemented-only-in-djinn-agent-seam",
        title="ADR-049 skill discovery implemented only in djinn-agent seam",
        note_type="case",
        folder="cases",
        content=SKILL_DISCOVERY_CANONICAL,
    ),
    UpdateAction(
        permalink="cases/merged-semantic-retrieval-into-note-memory-search-without-changing-the-mcp-interface",
        title="Merged semantic retrieval into note memory search without changing the MCP interface",
        note_type="case",
        folder="cases",
        content=SEMANTIC_RETRIEVAL_CANONICAL,
    ),
    UpdateAction(
        permalink="pitfalls/some-planner-dispatches-omit-memory-write-edit-tools",
        title="Some planner dispatches omit memory write/edit tools",
        note_type="pitfall",
        folder="pitfalls",
        content=PLANNER_TOOL_PITFALL,
    ),
]

DEMOTIONS = [
    ReclassifyAction(
        source_permalink="cases/adr-roadmap-captured-sqlite-migration-seam-inventory-categories",
        title="Working Spec — ADR-055 SQLite seam inventory",
        note_type="design",
        folder="design",
        permalink="design/working-spec-adr-055-sqlite-seam-inventory",
        content=SQLITE_SEAM_WORKING_SPEC,
        tags_json='["working-spec","adr-055","cleanup"]',
    ),
    ReclassifyAction(
        source_permalink="cases/adr-055-integration-contract-for-per-task-knowledge-branching",
        title="Working Spec — ADR-055 task knowledge branching rollout",
        note_type="design",
        folder="design",
        permalink="design/working-spec-adr-055-task-knowledge-branching-rollout",
        content=BRANCHING_WORKING_SPEC,
        tags_json='["working-spec","adr-055","cleanup"]',
    ),
]

ARCHIVE_PERMALINKS = [
    "cases/adr-049-skill-discovery-limited-to-djinn-agent-seam",
    "cases/adr-049-skill-discovery-updated-in-djinn-agent-seam-only",
    "cases/blend-semantic-retrieval-into-existing-note-search-without-changing-the-mcp-interface",
    "cases/added-vector-aware-ranking-to-the-existing-note-search-pipeline",
    "cases/blend-semantic-vector-search-into-existing-memory-search-ranking",
    "cases/thread-semantic-search-context-through-bridge-and-state-layers",
]


def slugify(title: str) -> str:
    slug = "".join(ch.lower() if ch.isalnum() or ch == "-" else "-" for ch in title)
    return "-".join(part for part in slug.split("-") if part)


REQUIRED_SECTIONS = {
    "pattern": [
        "## Context",
        "## Problem shape",
        "## Recommended approach",
        "## Why it works",
        "## Tradeoffs / limits",
        "## When to use",
        "## When not to use",
        "## Related",
    ],
    "pitfall": [
        "## Trigger / smell",
        "## Failure mode",
        "## Observable symptoms",
        "## Prevention",
        "## Recovery",
        "## Related",
    ],
    "case": [
        "## Situation",
        "## Constraint",
        "## Approach taken",
        "## Result",
        "## Why it worked / failed",
        "## Reusable lesson",
        "## Related",
    ],
}


def note_content_hash(content: str) -> str:
    return hashlib.sha256(content.encode("utf-8")).hexdigest()


def extract_wikilinks(content: str) -> list[tuple[str, str | None]]:
    links: list[tuple[str, str | None]] = []
    for match in WIKILINK_RE.finditer(content):
        target = (match.group(1) or "").strip()
        if not target:
            continue
        display = (match.group(2) or "").strip() or None
        links.append((target, display))
    return links


def resolve_target_id(con: sqlite3.Connection, project_id: str, raw_target: str) -> str | None:
    row = con.execute(
        "select id from notes where project_id=? and (permalink=? or title=?) order by case when permalink=? then 0 else 1 end limit 1",
        (project_id, raw_target, raw_target, raw_target),
    ).fetchone()
    return row[0] if row else None


def reindex_links_for_note(con: sqlite3.Connection, note_id: str, project_id: str, content: str) -> None:
    con.execute("delete from note_links where source_id=?", (note_id,))
    for raw_target, display in extract_wikilinks(content):
        link_id = con.execute("select lower(hex(randomblob(16)))").fetchone()[0]
        target_id = resolve_target_id(con, project_id, raw_target)
        con.execute(
            "insert or replace into note_links (id, source_id, target_id, target_raw, display_text) values (?, ?, ?, ?, ?)",
            (link_id, note_id, target_id, raw_target, display),
        )


def recompute_target_links(con: sqlite3.Connection, project_id: str, note_id: str, title: str, permalink: str) -> None:
    con.execute(
        "update note_links set target_id=? where source_id != ? and target_raw in (?, ?)",
        (note_id, note_id, title, permalink),
    )


def paragraphs(content: str) -> int:
    return len([block for block in content.split("\n\n") if block.strip()])


def looks_task_local(title: str, content: str) -> bool:
    haystack = f"{title}\n{content}".lower()
    return any(token in haystack for token in ["current task", "this session", "next session", "working spec"])


def family_counts(con: sqlite3.Connection, permalinks: Iterable[str]) -> int:
    placeholders = ",".join("?" for _ in permalinks)
    rows = con.execute(
        f"select count(*) from notes where permalink in ({placeholders})", tuple(permalinks)
    ).fetchone()[0]
    return int(rows)


def load_extracted_rows(con: sqlite3.Connection, project_id: str) -> list[dict[str, object]]:
    rows = con.execute(
        "select id, permalink, title, note_type, confidence, content from notes where project_id=? and note_type in ('case','pattern','pitfall') order by note_type, permalink",
        (project_id,),
    ).fetchall()
    return [dict(row) for row in rows]


def heuristic_audit_rows(rows: list[dict[str, object]]) -> dict[str, int]:
    underspecified = 0
    demote = 0
    archive = 0
    for row in rows:
        note_type = str(row["note_type"])
        content = str(row["content"])
        title = str(row["title"])
        required = REQUIRED_SECTIONS[note_type]
        missing = [section for section in required if section not in content]
        para_count = paragraphs(content)
        content_len = len(content.strip())
        has_footer_only_shape = (
            EXTRACTED_FOOTER in content
            and para_count <= 2
            and len(missing) == len(required)
        )
        if missing or content_len < 220 or para_count < 3:
            underspecified += 1
        if looks_task_local(title, content):
            demote += 1
        if has_footer_only_shape:
            archive += 1
    return {
        "scanned_note_count": len(rows),
        "underspecified_rough": underspecified,
        "demote_rough": demote,
        "archive_candidates_rough": archive,
    }


def heuristic_audit(con: sqlite3.Connection, project_id: str) -> dict[str, int]:
    return heuristic_audit_rows(load_extracted_rows(con, project_id))


def simulate_rows(rows: list[dict[str, object]]) -> list[dict[str, object]]:
    simulated = [dict(row) for row in rows]
    by_permalink = {str(row["permalink"]): row for row in simulated}

    for action in CANONICAL_UPDATES:
        row = by_permalink.get(action.permalink)
        if row is None:
            continue
        row["title"] = action.title
        row["note_type"] = action.note_type
        row["content"] = action.content

    for action in DEMOTIONS:
        row = by_permalink.pop(action.source_permalink, None)
        if row is None:
            continue
        simulated.remove(row)

    for permalink in ARCHIVE_PERMALINKS:
        row = by_permalink.pop(permalink, None)
        if row is None:
            continue
        simulated.remove(row)

    return simulated


def fetch_project_id(con: sqlite3.Connection, project_path: str) -> str:
    row = con.execute("select id from projects where path=?", (project_path,)).fetchone()
    if not row:
        raise SystemExit(f"project not found for path: {project_path}")
    return row[0]


def row_by_permalink(con: sqlite3.Connection, project_id: str, permalink: str) -> sqlite3.Row:
    row = con.execute(
        "select id, project_id, permalink, title, note_type, folder, tags, content from notes where project_id=? and permalink=?",
        (project_id, permalink),
    ).fetchone()
    if not row:
        raise SystemExit(f"note not found: {permalink}")
    return row


def apply_updates(con: sqlite3.Connection, project_id: str) -> dict[str, int]:
    strengthened = 0
    demoted = 0
    archived = 0

    for action in CANONICAL_UPDATES:
        row = row_by_permalink(con, project_id, action.permalink)
        tags_json = action.tags_json or row["tags"]
        content_hash = note_content_hash(action.content)
        con.execute(
            "update notes set title=?, note_type=?, folder=?, content=?, tags=?, content_hash=?, updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') where id=?",
            (
                action.title,
                action.note_type,
                action.folder,
                action.content,
                tags_json,
                content_hash,
                row["id"],
            ),
        )
        reindex_links_for_note(con, row["id"], project_id, action.content)
        recompute_target_links(con, project_id, row["id"], action.title, row["permalink"])
        strengthened += 1

    for action in DEMOTIONS:
        row = row_by_permalink(con, project_id, action.source_permalink)
        con.execute(
            "update notes set title=?, note_type=?, folder=?, permalink=?, content=?, tags=?, content_hash=?, updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') where id=?",
            (
                action.title,
                action.note_type,
                action.folder,
                action.permalink,
                action.content,
                action.tags_json,
                note_content_hash(action.content),
                row["id"],
            ),
        )
        reindex_links_for_note(con, row["id"], project_id, action.content)
        recompute_target_links(con, project_id, row["id"], action.title, action.permalink)
        demoted += 1

    for permalink in ARCHIVE_PERMALINKS:
        row = con.execute(
            "select id from notes where project_id=? and permalink=?",
            (project_id, permalink),
        ).fetchone()
        if row:
            con.execute("delete from notes where id=?", (row[0],))
            archived += 1

    return {
        "strengthened": strengthened,
        "demoted_to_working_spec": demoted,
        "archived": archived,
    }


def targeted_status(con: sqlite3.Connection, project_id: str) -> dict[str, object]:
    skill_family = [
        "cases/adr-049-skill-discovery-implemented-only-in-djinn-agent-seam",
        "cases/adr-049-skill-discovery-limited-to-djinn-agent-seam",
        "cases/adr-049-skill-discovery-updated-in-djinn-agent-seam-only",
    ]
    semantic_family = [
        "cases/merged-semantic-retrieval-into-note-memory-search-without-changing-the-mcp-interface",
        "cases/blend-semantic-retrieval-into-existing-note-search-without-changing-the-mcp-interface",
        "cases/added-vector-aware-ranking-to-the-existing-note-search-pipeline",
        "cases/blend-semantic-vector-search-into-existing-memory-search-ranking",
        "cases/thread-semantic-search-context-through-bridge-and-state-layers",
    ]
    demoted_targets = [action.permalink for action in DEMOTIONS]
    archive_targets = ARCHIVE_PERMALINKS
    return {
        "skill_discovery_family_rows": family_counts(con, skill_family),
        "semantic_retrieval_family_rows": family_counts(con, semantic_family),
        "demoted_working_spec_rows": family_counts(con, demoted_targets),
        "remaining_archive_targets": family_counts(con, archive_targets),
        "planner_tool_pitfall_present": bool(
            con.execute(
                "select 1 from notes where project_id=? and permalink='pitfalls/some-planner-dispatches-omit-memory-write-edit-tools'",
                (project_id,),
            ).fetchone()
        ),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", required=True, help="Path to djinn SQLite database")
    parser.add_argument("--project", default=PROJECT_PATH_DEFAULT, help="Canonical project path")
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--dry-run", action="store_true")
    mode.add_argument("--apply", action="store_true")
    args = parser.parse_args()

    con = sqlite3.connect(args.db)
    con.row_factory = sqlite3.Row
    project_id = fetch_project_id(con, args.project)

    extracted_rows = load_extracted_rows(con, project_id)
    before = {
        "heuristic_audit": heuristic_audit_rows(extracted_rows),
        "targeted_status": targeted_status(con, project_id),
    }
    projected_after = {
        "heuristic_audit": heuristic_audit_rows(simulate_rows(extracted_rows)),
        "targeted_status": {
            "skill_discovery_family_rows": 1,
            "semantic_retrieval_family_rows": 1,
            "demoted_working_spec_rows": len(DEMOTIONS),
            "remaining_archive_targets": 0,
            "planner_tool_pitfall_present": True,
        },
    }

    result = {
        "mode": "dry_run" if args.dry_run else "apply",
        "project_id": project_id,
        "before": before,
        "projected_after": projected_after,
    }

    if args.apply:
        try:
            with con:
                applied = apply_updates(con, project_id)
            after = {
                "heuristic_audit": heuristic_audit(con, project_id),
                "targeted_status": targeted_status(con, project_id),
            }
            result["applied"] = applied
            result["after"] = after
        except sqlite3.OperationalError as exc:
            result["error"] = str(exc)
            result["hint"] = (
                "The selected database is not writable. Use --dry-run for measurement, "
                "or run --apply against a writable database copy / maintenance instance."
            )
            print(json.dumps(result, indent=2, sort_keys=True))
            sys.exit(1)

    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
