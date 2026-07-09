#!/usr/bin/env python3
"""Generate deterministic Phase 1 fixture files for djinn-memory-eval.

Produces:
  fixtures/corpus-notes.jsonl
  fixtures/memory-ref-queries.jsonl
  fixtures/bad-cases.jsonl
  fixtures/manifest.json

Embeddings are computed deterministically using the same SHA-256 expand
algorithm as `DeterministicEmbeddingProvider` in the Rust crate.
"""

import hashlib
import json
import os
import struct
from datetime import datetime

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
CRATE_ROOT = os.path.dirname(SCRIPT_DIR)
FIXTURES_DIR = os.path.join(CRATE_ROOT, "fixtures")

EMBEDDING_DIM = 8


def deterministic_content_hash(text):
    """Compute a hex SHA-256 content hash of normalized text."""
    normalized = text.replace("\r\n", "\n").replace("\r", "\n").strip()
    return hashlib.sha256(normalized.encode("utf-8")).hexdigest()


def expand_hash_to_vector(hash_bytes, dimension):
    """Expand SHA-256 digest into a fixed-dimension L2-normalised f32 vector."""
    values = []
    counter = 0
    while len(values) < dimension:
        input_data = hash_bytes + struct.pack(">I", counter)
        expanded = hashlib.sha256(input_data).digest()
        for i in range(0, len(expanded), 4):
            if len(values) >= dimension:
                break
            chunk = expanded[i:i+4]
            bits = struct.unpack(">I", chunk)[0]
            value = (bits / 0xFFFFFFFF) * 2.0 - 1.0
            values.append(value)
        counter += 1

    # L2-normalise
    norm = sum(v * v for v in values) ** 0.5
    if norm > 0:
        values = [v / norm for v in values]
    return values


def compute_embedding(text, dimension=EMBEDDING_DIM):
    """Compute a deterministic embedding for text content."""
    normalized = text.replace("\r\n", "\n").replace("\r", "\n").strip()
    content_hash = deterministic_content_hash(text)
    hash_bytes = hashlib.sha256(normalized.encode("utf-8")).digest()
    vector = expand_hash_to_vector(hash_bytes, dimension)
    # Round to 6 decimal places for compact JSON
    vector = [round(v, 6) for v in vector]
    return {
        "content_hash": content_hash,
        "model_version": "deterministic-v1",
        "embedding_dim": dimension,
        "vector": vector,
    }


def make_note(permalink, title, content, note_type, folder, tags,
              timestamps, labels=None, graph_edges=None,
              expected_signals=None, status="active", confidence=1.0,
              retrieval_anchor=None):
    """Build a corpus note row."""
    embedding = compute_embedding(content)
    return {
        "permalink": permalink,
        "title": title,
        "content": content,
        "note_type": note_type,
        "folder": folder,
        "status": status,
        "tags": tags,
        "retrieval_anchor": retrieval_anchor,
        "timestamps": timestamps,
        "confidence": confidence,
        "embedding": embedding,
        "labels": labels or [],
        "graph_edges": graph_edges or [],
        "expected_signals": expected_signals or {},
    }


def make_query(query_id, query_text, memory_refs, expected_signals,
               task_id=None):
    """Build a mined memory-ref query row."""
    return {
        "query_id": query_id,
        "query_text": query_text,
        "task_id": task_id,
        "memory_refs": memory_refs,
        "expected_signals": expected_signals,
    }


def make_bad_case(case_id, query_text, case_type, expected_behavior,
                  relevant_note_permalinks, expected_signals,
                  task_id=None, tags=None):
    """Build a bad-case row."""
    return {
        "case_id": case_id,
        "query_text": query_text,
        "case_type": case_type,
        "expected_behavior": expected_behavior,
        "task_id": task_id,
        "relevant_note_permalinks": relevant_note_permalinks,
        "expected_signals": expected_signals,
        "tags": tags or [],
    }


# Timestamps
TS_RECENT = {
    "created_at": "2026-06-01T10:00:00.000Z",
    "updated_at": "2026-07-01T14:30:00.000Z",
    "last_accessed": "2026-07-08T09:00:00.000Z",
}
TS_MODERATE = {
    "created_at": "2026-04-15T08:00:00.000Z",
    "updated_at": "2026-05-20T12:00:00.000Z",
    "last_accessed": "2026-06-01T10:00:00.000Z",
}
TS_MATURE = {
    "created_at": "2026-03-01T00:00:00.000Z",
    "updated_at": "2026-04-01T00:00:00.000Z",
    "last_accessed": "2026-05-01T00:00:00.000Z",
}
TS_OLD_BUT_RELEVANT = {
    "created_at": "2025-06-01T00:00:00.000Z",
    "updated_at": "2025-09-01T00:00:00.000Z",
    "last_accessed": "2025-10-01T00:00:00.000Z",
}
TS_ANCIENT = {
    "created_at": "2025-01-15T00:00:00.000Z",
    "updated_at": "2025-04-01T00:00:00.000Z",
    "last_accessed": "2025-05-01T00:00:00.000Z",
}


def build_corpus():
    notes = []

    # Group A: Slot lifecycle patterns
    notes.append(make_note(
        permalink="patterns/supervisor-guard",
        title="Supervisor guard pattern for slot lifecycle",
        content="When a slot supervisor manages lifecycle transitions, a guard "
                "pattern prevents concurrent setup and teardown operations. The "
                "supervisor must acquire a lifecycle guard before initiating any "
                "state transition. If the guard is already held, the operation "
                "waits or fails fast. This prevents race conditions in slot "
                "lifecycle management.",
        note_type="pattern", folder="patterns",
        tags=["guard", "supervisor", "lifecycle", "slot"],
        timestamps=TS_RECENT,
        retrieval_anchor="supervisor guard for slot lifecycle transitions",
        labels=[
            {"entity_type": "concept", "name": "guard pattern"},
            {"entity_type": "technology", "name": "slot supervisor"},
        ],
        graph_edges=[
            {"source_permalink": "patterns/supervisor-guard",
             "target_permalink": "patterns/lifecycle-mgmt", "kind": "builds_on",
             "weight": 1.0},
            {"source_permalink": "patterns/supervisor-guard",
             "target_permalink": "pitfalls/race-condition", "kind": "contradicts",
             "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    notes.append(make_note(
        permalink="patterns/lifecycle-mgmt",
        title="Slot lifecycle management patterns",
        content="Slot lifecycle management covers the full lifecycle from creation "
                "through setup, running, teardown, and release. Each phase has "
                "invariants that must hold. The setup phase configures the slot "
                "environment, the running phase executes the task, and teardown "
                "cleans up resources. Proper ordering prevents resource leaks.",
        note_type="pattern", folder="patterns",
        tags=["lifecycle", "slot", "management"],
        timestamps=TS_MODERATE,
        retrieval_anchor="slot lifecycle phases and transitions",
        labels=[
            {"entity_type": "concept", "name": "lifecycle management"},
            {"entity_type": "technology", "name": "slot supervisor"},
        ],
        graph_edges=[
            {"source_permalink": "patterns/lifecycle-mgmt",
             "target_permalink": "adr/slot-status-model", "kind": "derived_from",
             "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    notes.append(make_note(
        permalink="patterns/slot-testing",
        title="Testing slot lifecycle transitions",
        content="Testing slot lifecycle transitions requires simulating concurrent "
                "setup and teardown operations. Use deterministic test harnesses "
                "that control the order of lifecycle callbacks. Assert that guard "
                "violations are detected and that resource cleanup is idempotent.",
        note_type="pattern", folder="patterns",
        tags=["testing", "slot", "lifecycle"],
        timestamps=TS_MODERATE,
        retrieval_anchor="test harness for slot lifecycle transitions",
        labels=[
            {"entity_type": "concept", "name": "testing"},
            {"entity_type": "technology", "name": "slot supervisor"},
        ],
        graph_edges=[
            {"source_permalink": "patterns/slot-testing",
             "target_permalink": "patterns/supervisor-guard",
             "kind": "exemplifies", "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    # Group B: Architecture decisions
    notes.append(make_note(
        permalink="adr/slot-status-model",
        title="ADR: Slot status state machine",
        content="Decision: model slot status as an explicit state machine with "
                "transitions: Created to Setup to Running to TearingDown to Released. "
                "Each transition is atomic and guarded. Invalid transitions raise "
                "SlotStatusViolation. The state machine is enforced in the supervisor.",
        note_type="adr", folder="decisions",
        tags=["slot", "state-machine", "adr"],
        timestamps=TS_RECENT,
        retrieval_anchor="slot status state machine decision",
        labels=[
            {"entity_type": "concept", "name": "state machine"},
            {"entity_type": "technology", "name": "slot supervisor"},
        ],
        graph_edges=[
            {"source_permalink": "adr/slot-status-model",
             "target_permalink": "patterns/lifecycle-mgmt", "kind": "supersedes",
             "weight": 1.0},
            {"source_permalink": "adr/slot-status-model",
             "target_permalink": "pitfalls/race-condition", "kind": "contradicts",
             "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    notes.append(make_note(
        permalink="adr/memory-decay-policy",
        title="ADR: Memory note decay and staleness policy",
        content="Decision: notes older than 90 days since last_accessed are subject "
                "to decay scoring in the retrieval pipeline. The decay function "
                "reduces the temporal relevance score progressively. Notes with high "
                "Bayesian confidence resist decay longer. Archived notes are excluded "
                "from active search but remain in the corpus for historical queries.",
        note_type="adr", folder="decisions",
        tags=["memory", "decay", "policy", "adr"],
        timestamps=TS_MATURE,
        retrieval_anchor="memory note decay and staleness policy",
        labels=[
            {"entity_type": "concept", "name": "decay policy"},
            {"entity_type": "concept", "name": "memory management"},
        ],
        graph_edges=[
            {"source_permalink": "adr/memory-decay-policy",
             "target_permalink": "reference/decay-thresholds",
             "kind": "derived_from", "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    notes.append(make_note(
        permalink="adr/rrf-fusion-strategy",
        title="ADR: RRF fusion strategy for multi-signal retrieval",
        content="Decision: use Reciprocal Rank Fusion to combine ranking signals "
                "from lexical search, semantic embeddings, temporal decay, graph "
                "proximity, entity overlap, and task affinity. Each signal produces "
                "an independent ranked list. RRF merges them with tunable weights "
                "per signal type.",
        note_type="adr", folder="decisions",
        tags=["rrf", "fusion", "retrieval", "adr"],
        timestamps=TS_RECENT,
        retrieval_anchor="RRF fusion strategy for multi-signal retrieval",
        labels=[
            {"entity_type": "concept", "name": "RRF fusion"},
            {"entity_type": "concept", "name": "multi-signal retrieval"},
        ],
        graph_edges=[
            {"source_permalink": "adr/rrf-fusion-strategy",
             "target_permalink": "reference/decay-thresholds",
             "kind": "builds_on", "weight": 1.0},
            {"source_permalink": "adr/rrf-fusion-strategy",
             "target_permalink": "patterns/supervisor-guard",
             "kind": "co_access", "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    # Group C: Cases
    notes.append(make_note(
        permalink="cases/slot-lifecycle-race",
        title="Slot lifecycle race condition case study",
        content="During a production incident, a slot was torn down while the "
                "supervisor was still processing setup callbacks. The lifecycle "
                "runner observed a SlotStatus Released guard violation. Root cause: "
                "missing guard acquisition before the teardown path. Fix: enforce "
                "guard acquisition in all lifecycle transition entry points.",
        note_type="case", folder="cases",
        tags=["race-condition", "slot", "lifecycle", "incident"],
        timestamps=TS_RECENT,
        retrieval_anchor="slot teardown race during supervisor setup",
        labels=[
            {"entity_type": "concept", "name": "race condition"},
            {"entity_type": "technology", "name": "slot supervisor"},
            {"entity_type": "file", "name": "slot/lifecycle.rs"},
        ],
        graph_edges=[
            {"source_permalink": "cases/slot-lifecycle-race",
             "target_permalink": "patterns/supervisor-guard",
             "kind": "derived_from", "weight": 1.0},
            {"source_permalink": "cases/slot-lifecycle-race",
             "target_permalink": "pitfalls/race-condition",
             "kind": "exemplifies", "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True, "task_affinity": True,
        },
    ))

    notes.append(make_note(
        permalink="cases/deployment-rollback",
        title="Deployment rollback procedure for slot failures",
        content="When a slot fails during deployment, the rollback procedure must "
                "release all acquired resources in reverse order. The supervisor "
                "initiates teardown, waits for cleanup completion, then marks the "
                "deployment as rolled back. Monitoring must detect stuck rollbacks "
                "within 5 minutes.",
        note_type="case", folder="cases",
        tags=["deployment", "rollback", "slot"],
        timestamps=TS_MODERATE,
        retrieval_anchor="deployment rollback for slot failure",
        labels=[
            {"entity_type": "concept", "name": "rollback"},
            {"entity_type": "technology", "name": "slot supervisor"},
        ],
        graph_edges=[
            {"source_permalink": "cases/deployment-rollback",
             "target_permalink": "patterns/lifecycle-mgmt",
             "kind": "builds_on", "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    notes.append(make_note(
        permalink="cases/deadlock-recovery",
        title="Deadlock recovery in multi-slot orchestration",
        content="A deadlock was observed when two slots each held a resource the "
                "other needed. The fix introduces a global resource ordering and "
                "timeout-based deadlock detection. When a deadlock is detected, "
                "the youngest slot releases its resources and retries.",
        note_type="case", folder="cases",
        tags=["deadlock", "recovery", "orchestration"],
        timestamps=TS_MATURE,
        retrieval_anchor="deadlock recovery in multi-slot orchestration",
        labels=[
            {"entity_type": "concept", "name": "deadlock"},
            {"entity_type": "technology", "name": "slot supervisor"},
        ],
        graph_edges=[
            {"source_permalink": "cases/deadlock-recovery",
             "target_permalink": "patterns/supervisor-guard",
             "kind": "derived_from", "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    # Group D: References
    notes.append(make_note(
        permalink="reference/decay-thresholds",
        title="Temporal decay threshold reference",
        content="The temporal decay function uses these thresholds: notes accessed "
                "within 7 days get full temporal score. Notes 7-30 days old get "
                "80 percent score. Notes 30-90 days old get 50 percent score. Notes "
                "older than 90 days get 20 percent score. The decay function is "
                "applied as a multiplicative factor to the temporal signal.",
        note_type="reference", folder="references",
        tags=["decay", "thresholds", "temporal", "reference"],
        timestamps=TS_MATURE,
        retrieval_anchor="temporal decay function thresholds",
        labels=[
            {"entity_type": "concept", "name": "decay policy"},
        ],
        graph_edges=[
            {"source_permalink": "reference/decay-thresholds",
             "target_permalink": "adr/memory-decay-policy",
             "kind": "derived_from", "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    notes.append(make_note(
        permalink="reference/slot-api-reference",
        title="Slot API reference documentation",
        content="The Slot API provides methods for creating, configuring, and "
                "destroying slots. Key methods include create slot, setup slot, "
                "run task, teardown slot, and release slot. Each method validates "
                "the current slot status before proceeding.",
        note_type="reference", folder="references",
        tags=["api", "slot", "reference"],
        timestamps=TS_RECENT,
        retrieval_anchor="slot API methods and lifecycle",
        labels=[
            {"entity_type": "technology", "name": "slot API"},
        ],
        graph_edges=[
            {"source_permalink": "reference/slot-api-reference",
             "target_permalink": "patterns/lifecycle-mgmt",
             "kind": "derived_from", "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    notes.append(make_note(
        permalink="reference/graph-scoring",
        title="Graph proximity scoring reference",
        content="Graph proximity scoring uses note associations including co_access, "
                "builds_on, contradicts, supersedes, exemplifies, derived_from, and "
                "wikilink to compute distance-based relevance. Notes within 2 hops "
                "of the seed note receive proximity boosting. The boost decays "
                "exponentially with hop distance.",
        note_type="reference", folder="references",
        tags=["graph", "scoring", "proximity", "reference"],
        timestamps=TS_MODERATE,
        retrieval_anchor="graph proximity scoring algorithm",
        labels=[
            {"entity_type": "concept", "name": "graph scoring"},
            {"entity_type": "concept", "name": "RRF fusion"},
        ],
        graph_edges=[
            {"source_permalink": "reference/graph-scoring",
             "target_permalink": "adr/rrf-fusion-strategy",
             "kind": "builds_on", "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    # Group E: Pitfalls
    notes.append(make_note(
        permalink="pitfalls/race-condition",
        title="Common race condition pitfalls in slot management",
        content="Race conditions in slot management occur when lifecycle transitions "
                "are not properly guarded. Common pitfalls include starting setup "
                "before the previous teardown completes, releasing resources in the "
                "wrong order, not checking slot status before operations, and "
                "assuming single-threaded execution with async callbacks.",
        note_type="pitfall", folder="pitfalls",
        tags=["race-condition", "pitfall", "slot", "lifecycle"],
        timestamps=TS_MODERATE,
        retrieval_anchor="race condition pitfalls in slot lifecycle",
        labels=[
            {"entity_type": "concept", "name": "race condition"},
        ],
        graph_edges=[
            {"source_permalink": "pitfalls/race-condition",
             "target_permalink": "patterns/supervisor-guard",
             "kind": "contradicts", "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    notes.append(make_note(
        permalink="pitfalls/embedding-staleness",
        title="Embedding staleness pitfall in note retrieval",
        content="When note content is updated but embeddings are not recomputed, "
                "the semantic search returns stale results. The fix is to trigger "
                "embedding recomputation on every note update. The deterministic "
                "embedder uses content hashing to detect when recomputation is needed.",
        note_type="pitfall", folder="pitfalls",
        tags=["embedding", "staleness", "pitfall"],
        timestamps=TS_MATURE,
        retrieval_anchor="embedding staleness detection",
        labels=[
            {"entity_type": "concept", "name": "embedding staleness"},
            {"entity_type": "concept", "name": "memory management"},
        ],
        graph_edges=[
            {"source_permalink": "pitfalls/embedding-staleness",
             "target_permalink": "adr/memory-decay-policy",
             "kind": "exemplifies", "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    # Group F: Research
    notes.append(make_note(
        permalink="research/embedding-quality",
        title="Embedding quality study for memory retrieval",
        content="A study comparing embedding models for memory note retrieval found "
                "that sentence-transformer models outperform TF-IDF for semantic "
                "similarity but underperform for exact keyword matching. The hybrid "
                "approach using RRF fusion of both achieves the best recall scores.",
        note_type="research", folder="research",
        tags=["embedding", "quality", "study", "research"],
        timestamps=TS_MATURE,
        retrieval_anchor="embedding model comparison for memory retrieval",
        labels=[
            {"entity_type": "concept", "name": "embedding quality"},
            {"entity_type": "concept", "name": "RRF fusion"},
        ],
        graph_edges=[
            {"source_permalink": "research/embedding-quality",
             "target_permalink": "adr/rrf-fusion-strategy",
             "kind": "builds_on", "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    # Group G: Over-decay-threshold notes (>90 days since last_accessed)
    notes.append(make_note(
        permalink="cases/over-decay-slot-setup",
        title="Slot setup failure in cold start scenario",
        content="During cold start, a slot setup failed because the supervisor "
                "did not wait for the environment configuration to propagate. The "
                "fix adds a readiness check before proceeding with setup. This "
                "pattern is critical for preventing cascading failures during "
                "system startup.",
        note_type="case", folder="cases",
        tags=["cold-start", "setup", "failure"],
        timestamps=TS_OLD_BUT_RELEVANT,
        confidence=0.75,
        retrieval_anchor="slot setup failure during cold start",
        labels=[
            {"entity_type": "concept", "name": "cold start"},
            {"entity_type": "technology", "name": "slot supervisor"},
        ],
        graph_edges=[
            {"source_permalink": "cases/over-decay-slot-setup",
             "target_permalink": "patterns/lifecycle-mgmt",
             "kind": "derived_from", "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    notes.append(make_note(
        permalink="cases/legacy-migration",
        title="Legacy slot migration to new lifecycle model",
        content="Migrating legacy slots from the old flat lifecycle to the new "
                "state-machine model required shimming old status codes. The "
                "migration is irreversible and must be run during a maintenance "
                "window. Rollback is not possible once the migration commits.",
        note_type="case", folder="cases",
        tags=["migration", "legacy", "slot"],
        timestamps=TS_ANCIENT,
        confidence=0.6,
        retrieval_anchor="legacy slot migration procedure",
        labels=[
            {"entity_type": "concept", "name": "migration"},
            {"entity_type": "technology", "name": "slot supervisor"},
        ],
        graph_edges=[
            {"source_permalink": "cases/legacy-migration",
             "target_permalink": "adr/slot-status-model",
             "kind": "derived_from", "weight": 1.0},
            {"source_permalink": "cases/legacy-migration",
             "target_permalink": "pitfalls/race-condition",
             "kind": "exemplifies", "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    # Group H: Cleanup/utility
    notes.append(make_note(
        permalink="patterns/cleanup-procedures",
        title="Resource cleanup procedures for slot teardown",
        content="Resource cleanup during slot teardown must be idempotent and "
                "ordered. Close network connections first, then flush buffers, "
                "then release file handles, then free memory. Each step logs its "
                "completion. If a step fails, the remaining steps still execute "
                "to prevent resource leaks.",
        note_type="pattern", folder="patterns",
        tags=["cleanup", "teardown", "resource-management"],
        timestamps=TS_RECENT,
        retrieval_anchor="ordered resource cleanup for slot teardown",
        labels=[
            {"entity_type": "concept", "name": "resource cleanup"},
            {"entity_type": "technology", "name": "slot supervisor"},
        ],
        graph_edges=[
            {"source_permalink": "patterns/cleanup-procedures",
             "target_permalink": "patterns/lifecycle-mgmt",
             "kind": "builds_on", "weight": 1.0},
            {"source_permalink": "patterns/cleanup-procedures",
             "target_permalink": "cases/deployment-rollback",
             "kind": "co_access", "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    notes.append(make_note(
        permalink="pitfalls/common-operator-errors",
        title="Common operator errors in slot management",
        content="Operators commonly make these errors: forcing slot release without "
                "teardown, modifying slot configuration while running, ignoring "
                "guard violation warnings, and not monitoring cleanup completion. "
                "Each error can cause data corruption or resource leaks.",
        note_type="pitfall", folder="pitfalls",
        tags=["operator", "errors", "pitfall"],
        timestamps=TS_MODERATE,
        retrieval_anchor="common operator errors with slots",
        labels=[
            {"entity_type": "concept", "name": "operator error"},
            {"entity_type": "technology", "name": "slot supervisor"},
        ],
        graph_edges=[
            {"source_permalink": "pitfalls/common-operator-errors",
             "target_permalink": "patterns/cleanup-procedures",
             "kind": "builds_on", "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    # Group I: Graph-heavy note
    notes.append(make_note(
        permalink="reference/supervisor-design-principles",
        title="Supervisor design principles",
        content="Core design principles for slot supervisors include single "
                "responsibility with one supervisor per slot pool, fail-fast on "
                "invalid transitions, observable state changes via event bus, "
                "idempotent cleanup, and graceful degradation under load. These "
                "principles guide all supervisor implementations.",
        note_type="reference", folder="references",
        tags=["supervisor", "design", "principles"],
        timestamps=TS_RECENT,
        retrieval_anchor="supervisor design principles",
        labels=[
            {"entity_type": "concept", "name": "design principles"},
            {"entity_type": "technology", "name": "slot supervisor"},
        ],
        graph_edges=[
            {"source_permalink": "reference/supervisor-design-principles",
             "target_permalink": "patterns/supervisor-guard",
             "kind": "builds_on", "weight": 1.0},
            {"source_permalink": "reference/supervisor-design-principles",
             "target_permalink": "patterns/lifecycle-mgmt",
             "kind": "builds_on", "weight": 1.0},
            {"source_permalink": "reference/supervisor-design-principles",
             "target_permalink": "adr/slot-status-model",
             "kind": "exemplifies", "weight": 1.0},
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    return notes


def build_memory_ref_queries():
    queries = []

    # Task-affinity queries (5)
    queries.append(make_query(
        query_id="task-slot-lifecycle-001",
        query_text="How to handle slot lifecycle race conditions?",
        task_id="slot-lifecycle-001",
        memory_refs=[
            "cases/slot-lifecycle-race",
            "patterns/supervisor-guard",
            "pitfalls/race-condition",
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True, "task_affinity": True,
        },
    ))
    queries.append(make_query(
        query_id="task-deploy-rollback-002",
        query_text="What is the deployment rollback procedure for slots?",
        task_id="deploy-rollback-002",
        memory_refs=[
            "cases/deployment-rollback",
            "patterns/cleanup-procedures",
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True, "task_affinity": True,
        },
    ))
    queries.append(make_query(
        query_id="task-guard-setup-003",
        query_text="Supervisor guard pattern implementation details",
        task_id="guard-setup-003",
        memory_refs=[
            "patterns/supervisor-guard",
            "reference/supervisor-design-principles",
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True, "task_affinity": True,
        },
    ))
    queries.append(make_query(
        query_id="task-rrf-config-004",
        query_text="How to configure RRF fusion weights for retrieval?",
        task_id="rrf-config-004",
        memory_refs=[
            "adr/rrf-fusion-strategy",
            "reference/graph-scoring",
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True, "task_affinity": True,
        },
    ))
    queries.append(make_query(
        query_id="task-decay-tuning-005",
        query_text="Memory note decay tuning and thresholds",
        task_id="decay-tuning-005",
        memory_refs=[
            "adr/memory-decay-policy",
            "reference/decay-thresholds",
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True, "task_affinity": True,
        },
    ))

    # Non-task-affinity queries (12)
    queries.append(make_query(
        query_id="q-lifecycle-patterns",
        query_text="What are the slot lifecycle management patterns?",
        memory_refs=[
            "patterns/lifecycle-mgmt",
            "patterns/supervisor-guard",
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))
    queries.append(make_query(
        query_id="q-testing-transitions",
        query_text="How to test slot lifecycle transitions?",
        memory_refs=[
            "patterns/slot-testing",
            "patterns/supervisor-guard",
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))
    queries.append(make_query(
        query_id="q-status-model",
        query_text="What is the slot status state machine design?",
        memory_refs=[
            "adr/slot-status-model",
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))
    queries.append(make_query(
        query_id="q-deadlock-handling",
        query_text="How to handle deadlocks in multi-slot orchestration?",
        memory_refs=[
            "cases/deadlock-recovery",
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))
    queries.append(make_query(
        query_id="q-embedding-quality",
        query_text="Which embedding model works best for note retrieval?",
        memory_refs=[
            "research/embedding-quality",
            "pitfalls/embedding-staleness",
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))
    queries.append(make_query(
        query_id="q-graph-scoring",
        query_text="How does graph proximity scoring work for notes?",
        memory_refs=[
            "reference/graph-scoring",
            "adr/rrf-fusion-strategy",
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))
    queries.append(make_query(
        query_id="q-cleanup-order",
        query_text="What is the correct resource cleanup order during teardown?",
        memory_refs=[
            "patterns/cleanup-procedures",
            "patterns/lifecycle-mgmt",
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))
    queries.append(make_query(
        query_id="q-operator-errors",
        query_text="What are common operator mistakes with slot management?",
        memory_refs=[
            "pitfalls/common-operator-errors",
            "pitfalls/race-condition",
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))
    queries.append(make_query(
        query_id="q-cold-start-setup",
        query_text="How to fix slot setup failures during cold start?",
        memory_refs=[
            "cases/over-decay-slot-setup",
            "patterns/lifecycle-mgmt",
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))
    queries.append(make_query(
        query_id="q-legacy-migration",
        query_text="How to migrate legacy slots to the new lifecycle model?",
        memory_refs=[
            "cases/legacy-migration",
            "adr/slot-status-model",
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))
    queries.append(make_query(
        query_id="q-supervisor-principles",
        query_text="What are the core design principles for slot supervisors?",
        memory_refs=[
            "reference/supervisor-design-principles",
            "patterns/supervisor-guard",
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))
    queries.append(make_query(
        query_id="q-api-methods",
        query_text="What API methods are available for slot management?",
        memory_refs=[
            "reference/slot-api-reference",
            "patterns/lifecycle-mgmt",
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
    ))

    return queries


def build_bad_cases():
    cases = []

    # Over-decay-threshold case
    cases.append(make_bad_case(
        case_id="bc-over-decay-001",
        query_text="Slot setup failure in cold start scenarios",
        case_type="over_decay_threshold",
        expected_behavior="Note should remain in recall@10 despite being older "
                          "than the 90-day decay threshold",
        relevant_note_permalinks=["cases/over-decay-slot-setup"],
        expected_signals={"vector": True, "lexical": True, "temporal": True},
        tags=["decay", "cold-start", "high-priority"],
    ))

    # Graph/entity-influenced cases (3)
    cases.append(make_bad_case(
        case_id="bc-graph-entity-001",
        query_text="Which patterns build on the supervisor guard pattern?",
        case_type="graph_entity_influenced",
        expected_behavior="Graph proximity or entity overlap should surface "
                          "connected notes in recall@5",
        relevant_note_permalinks=[
            "patterns/lifecycle-mgmt",
            "patterns/slot-testing",
        ],
        expected_signals={"lexical": True, "graph": True, "entity": True},
        tags=["graph", "entity", "supervisor"],
    ))
    cases.append(make_bad_case(
        case_id="bc-graph-entity-002",
        query_text="What contradictions exist in slot lifecycle design?",
        case_type="graph_entity_influenced",
        expected_behavior="Graph 'contradicts' edges should surface the race "
                          "condition pitfall",
        relevant_note_permalinks=[
            "pitfalls/race-condition",
            "cases/slot-lifecycle-race",
        ],
        expected_signals={"lexical": True, "graph": True, "entity": True},
        tags=["graph", "contradiction"],
    ))
    cases.append(make_bad_case(
        case_id="bc-graph-entity-003",
        query_text="Supervisor design principles and their derivation",
        case_type="graph_entity_influenced",
        expected_behavior="Graph traversal from supervisor-design-principles "
                          "should surface notes it builds_on",
        relevant_note_permalinks=[
            "patterns/supervisor-guard",
            "adr/slot-status-model",
        ],
        expected_signals={"lexical": True, "graph": True, "entity": True},
        tags=["graph", "derivation"],
    ))

    # Task-affinity-influenced cases (3)
    cases.append(make_bad_case(
        case_id="bc-task-affinity-001",
        query_text="What memory notes are associated with slot lifecycle work?",
        case_type="task_affinity_influenced",
        expected_behavior="Task-affinity signal should surface notes in the "
                          "task's memory_refs",
        task_id="slot-lifecycle-001",
        relevant_note_permalinks=["cases/slot-lifecycle-race"],
        expected_signals={"vector": True, "task_affinity": True},
        tags=["task-affinity", "slot-lifecycle"],
    ))
    cases.append(make_bad_case(
        case_id="bc-task-affinity-002",
        query_text="Deployment rollback notes for current task context",
        case_type="task_affinity_influenced",
        expected_behavior="Task-affinity should boost deployment-rollback note",
        task_id="deploy-rollback-002",
        relevant_note_permalinks=["cases/deployment-rollback"],
        expected_signals={"vector": True, "task_affinity": True},
        tags=["task-affinity", "deployment"],
    ))
    cases.append(make_bad_case(
        case_id="bc-task-affinity-003",
        query_text="RRF fusion configuration for current retrieval work",
        case_type="task_affinity_influenced",
        expected_behavior="Task-affinity should surface the RRF ADR",
        task_id="rrf-config-004",
        relevant_note_permalinks=["adr/rrf-fusion-strategy"],
        expected_signals={"vector": True, "task_affinity": True},
        tags=["task-affinity", "rrf"],
    ))

    # Zero-result case
    cases.append(make_bad_case(
        case_id="bc-zero-result-001",
        query_text="Quantum computing applications in slot management",
        case_type="zero_result",
        expected_behavior="No relevant notes exist. Zero-result rate should "
                          "not increase.",
        relevant_note_permalinks=[],
        expected_signals={},
        tags=["zero-result", "edge-case"],
    ))

    # Rank regression cases (2)
    cases.append(make_bad_case(
        case_id="bc-rank-regression-001",
        query_text="Resource cleanup procedures during teardown",
        case_type="rank_regression",
        expected_behavior="Cleanup procedures note should be in top-3 for "
                          "this query",
        relevant_note_permalinks=[
            "patterns/cleanup-procedures",
            "patterns/lifecycle-mgmt",
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
        },
        tags=["rank-regression", "cleanup"],
    ))
    cases.append(make_bad_case(
        case_id="bc-rank-regression-002",
        query_text="Guard violations and slot status checks",
        case_type="rank_regression",
        expected_behavior="Guard pattern and status model should be in top-5",
        relevant_note_permalinks=[
            "patterns/supervisor-guard",
            "adr/slot-status-model",
            "pitfalls/race-condition",
        ],
        expected_signals={
            "vector": True, "lexical": True, "temporal": True,
            "graph": True, "entity": True,
        },
        tags=["rank-regression", "guard"],
    ))

    return cases


def compute_file_hash(filepath):
    """Compute SHA-256 hex digest of a file."""
    sha = hashlib.sha256()
    with open(filepath, "rb") as f:
        for chunk in iter(lambda: f.read(8192), b""):
            sha.update(chunk)
    return sha.hexdigest()


def write_jsonl(filepath, rows):
    """Write rows as JSONL."""
    with open(filepath, "w") as f:
        for row in rows:
            f.write(json.dumps(row, separators=(",", ":")) + "\n")


def main():
    os.makedirs(FIXTURES_DIR, exist_ok=True)

    corpus = build_corpus()
    queries = build_memory_ref_queries()
    bad_cases = build_bad_cases()

    corpus_path = os.path.join(FIXTURES_DIR, "corpus-notes.jsonl")
    queries_path = os.path.join(FIXTURES_DIR, "memory-ref-queries.jsonl")
    bad_cases_path = os.path.join(FIXTURES_DIR, "bad-cases.jsonl")

    write_jsonl(corpus_path, corpus)
    write_jsonl(queries_path, queries)
    write_jsonl(bad_cases_path, bad_cases)

    corpus_hash = compute_file_hash(corpus_path)
    queries_hash = compute_file_hash(queries_path)
    bad_cases_hash = compute_file_hash(bad_cases_path)

    manifest = {
        "schema_version": "1.0.0",
        "created_at": datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%S.000Z"),
        "corpus_note_count": len(corpus),
        "memory_ref_query_count": len(queries),
        "bad_case_count": len(bad_cases),
        "file_hashes": {
            "corpus_notes": corpus_hash,
            "memory_ref_queries": queries_hash,
            "bad_cases": bad_cases_hash,
        },
    }
    manifest_path = os.path.join(FIXTURES_DIR, "manifest.json")
    with open(manifest_path, "w") as f:
        json.dump(manifest, f, indent=2)
        f.write("\n")

    print(f"Generated fixtures:")
    print(f"  corpus-notes.jsonl: {len(corpus)} notes")
    print(f"  memory-ref-queries.jsonl: {len(queries)} queries")
    print(f"  bad-cases.jsonl: {len(bad_cases)} bad cases")
    print(f"  manifest.json: SHA-256 hashes computed")


if __name__ == "__main__":
    main()
