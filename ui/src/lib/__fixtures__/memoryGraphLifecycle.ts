/**
 * Raw `memory_graph` lifecycle payloads for adapter runtime-guard tests.
 *
 * These deliberately have no request/response type annotations: they model
 * untrusted MCP wire data, including malformed values the adapter must handle.
 */

export const validLifecycleResponse = {
  nodes: [
    {
      id: "active-note",
      permalink: "notes/active-note",
      title: "Active note",
      note_type: "adr",
      folder: "notes",
      connection_count: 2,
      is_orphan: false,
      broken_targets: [],
      status: "active",
    },
    {
      id: "archived-note",
      permalink: "notes/archived-note",
      title: "Archived note",
      note_type: "reference",
      folder: "notes",
      connection_count: 2,
      is_orphan: false,
      broken_targets: [],
      status: "archived",
      lifecycle_changed_at: "2026-07-20T12:34:56.789Z",
    },
    {
      id: "deprecated-note",
      permalink: "notes/deprecated-note",
      title: "Deprecated note",
      note_type: "pitfall",
      folder: "notes",
      connection_count: 2,
      is_orphan: false,
      broken_targets: [],
      status: "deprecated",
      lifecycle_changed_at: "2026-07-19T08:00:00Z",
    },
  ],
  edges: [
    { source_id: "active-note", target_id: "archived-note", raw_text: "Archived note" },
    { source_id: "archived-note", target_id: "deprecated-note", raw_text: "Deprecated note" },
  ],
  typed_edges: [
    { source_id: "deprecated-note", target_id: "active-note", kind: "supersedes", weight: 1 },
  ],
  lifecycle_summary: {
    inactive_total: 2,
    inactive_returned: 2,
    inactive_omitted: 0,
  },
};

/** A valid response where the transition timestamp is explicitly null. */
export const nullLifecycleChangedAtResponse = {
  nodes: [
    {
      id: "archived-null-time",
      permalink: "notes/archived-null-time",
      title: "Archived without transition time",
      note_type: "reference",
      folder: "notes",
      connection_count: 0,
      is_orphan: true,
      broken_targets: [],
      status: "archived",
      lifecycle_changed_at: null,
    },
  ],
  edges: [],
};

/** A valid response where the transition timestamp is omitted entirely. */
export const omittedLifecycleChangedAtResponse = {
  nodes: [
    {
      id: "deprecated-missing-time",
      permalink: "notes/deprecated-missing-time",
      title: "Deprecated without transition time",
      note_type: "pitfall",
      folder: "notes",
      connection_count: 0,
      is_orphan: true,
      broken_targets: [],
      status: "deprecated",
    },
  ],
  edges: [],
};

/** A standalone valid summary response for summary-specific guard assertions. */
export const validLifecycleSummaryResponse = {
  nodes: [],
  edges: [],
  lifecycle_summary: {
    inactive_total: 5,
    inactive_returned: 2,
    inactive_omitted: 3,
  },
};

export const malformedLifecycleStatusResponse = {
  nodes: [{ id: "bad-status", status: "retired-forever" }],
  edges: [],
};

export const malformedLifecycleTimestampResponse = {
  nodes: [{ id: "bad-time", status: "archived", lifecycle_changed_at: "not-an-iso-timestamp" }],
  edges: [],
};

export const malformedLifecycleSummaryResponse = {
  nodes: [],
  edges: [],
  lifecycle_summary: {
    inactive_total: "two",
    inactive_returned: -1,
    inactive_omitted: Number.NaN,
  },
};

/** Non-array edge collections must receive the adapter's endpoint-safe defaults. */
export const malformedEdgeCollectionsResponse = {
  nodes: [{ id: "edge-source" }],
  edges: { source_id: "edge-source", target_id: "edge-target" },
  typed_edges: "not-an-edge-array",
};

/** Omitted edge collections also normalize to empty, endpoint-safe defaults. */
export const omittedEdgeCollectionsResponse = {
  nodes: [{ id: "edge-source" }],
};

/** Missing, null, and empty endpoints must not create usable graph edges. */
export const malformedEdgeEndpointsResponse = {
  nodes: [{ id: "edge-source" }, { id: "edge-target" }],
  edges: [
    { source_id: "edge-source", target_id: "", raw_text: "missing target" },
    { source_id: null, target_id: "edge-target", raw_text: "missing source" },
    { source_id: "edge-source", target_id: "edge-target", raw_text: "valid edge" },
  ],
  typed_edges: [
    { source_id: "edge-source", target_id: "", kind: "builds_on", weight: 1 },
    { source_id: "edge-source", target_id: "edge-target", kind: "", weight: 1 },
    { source_id: "edge-source", target_id: "edge-target", kind: "builds_on", weight: 1 },
  ],
};

/** The pre-lifecycle active-only response shape remains accepted unchanged. */
export const legacyActiveOnlyResponse = {
  nodes: [
    {
      id: "legacy-active",
      permalink: "notes/legacy-active",
      title: "Legacy active note",
      note_type: "adr",
      folder: "notes",
      connection_count: 1,
      is_orphan: false,
      broken_targets: [],
    },
  ],
  edges: [],
  typed_edges: [],
};
