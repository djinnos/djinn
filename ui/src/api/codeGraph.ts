/**
 * Typed wrappers for the `code_graph` MCP tool.
 *
 * The MCP autogen produces only `CodeGraphInput` (which is a single union of
 * every operation's args) and `CodeGraphOutput` (which is `Record<string, any>`,
 * because the Rust `CodeGraphResponse` enum is `#[serde(untagged)]`).
 *
 * For PR D1 we expose:
 *   - one tiny `callCodeGraph` helper that pins `project` + `operation` and
 *     forwards the rest of the autogen'd input,
 *   - one named wrapper per op the UI cares about for D2-D6.
 *
 * Response narrowing for the existing ops still lives in
 * `@/components/pulse/pulseTypes` — we re-export the parsers from here so the
 * `pulse/` directory survives only as a parser bag (the pulse panels and page
 * are gone in this PR).
 */

import { callMcpTool } from "@/api/mcpClient";
import type { CodeGraphOutput } from "@/api/generated/mcp-tools.gen";

/**
 * Operations the UI dispatches today. The server accepts more (see the
 * `CodeGraphInput.operation` doc-comment in `mcp-tools.gen.ts`) — add to
 * this union as new ops land. Keeping the union narrow gives editor
 * autocompletion at call sites.
 */
export type CodeGraphOperation =
  | "status"
  | "ranked"
  | "search"
  | "route_map"
  | "shape_check"
  | "api_impact"
  | "flow"
  | "neighbors"
  | "impact"
  | "implementations"
  | "describe"
  | "context"
  | "cycles"
  | "orphans"
  | "path"
  | "edges"
  | "symbols_at"
  | "diff_touches"
  | "detect_changes"
  | "workspaces"
  | "coverage"
  | "snapshot";

/**
 * Semantic zoom level for the `snapshot` op. Mirrors the server-side
 * `SnapshotLevel { Symbol, Community }` enum. When omitted, the server
 * defaults to `"symbol"` — existing callers keep that behavior.
 */
export type SnapshotLevel = "symbol" | "community";

/**
 * Per-call extras. The autogen'd `CodeGraphInput` is one big union with an
 * `[k: string]: any` index signature, which makes `Pick<>`-derived helper
 * types unusable. We mirror the fields we touch here as plain optionals;
 * the server still validates per-op required-ness on receipt.
 *
 * Add fields as new ops/UI dispatches surface them.
 */
export interface CodeGraphArgs {
  changed_files?: string[];
  changed_ranges?: Array<{
    file: string;
    start_line: number;
    end_line?: number;
  }>;
  confidence?: string;
  direction?: string;
  edge_kind?: string;
  end_line?: number;
  file?: string;
  file_glob?: string;
  framework?: string;
  from?: string;
  from_glob?: string;
  from_sha?: string;
  group_by?: string;
  /** PR C1 `context` op: fetch the symbol body verbatim. Defaults false. */
  include_content?: boolean;
  /** Route/API ops: include optional response fields when checking shape drift. */
  include_optional?: boolean;
  key?: string;
  kind_filter?: string;
  kind_hint?: string;
  /** Semantic zoom level for the `snapshot` op (`"symbol" | "community"`). */
  level?: SnapshotLevel;
  limit?: number;
  max_depth?: number;
  max_files_per_commit?: number;
  min_confidence?: number;
  min_size?: number;
  /** PR B4 `search` op: `name` (legacy) | `lexical` | `semantic` | `structural` | `hybrid`. */
  mode?: string;
  module_glob?: string;
  /** PR C1 `context` op: short-name lookup target (alternative to `key`). */
  name?: string;
  /** Exact HTTP route path for route/API ops. */
  path?: string;
  /** Route path glob filter for `route_map`. */
  path_glob?: string;
  query?: string;
  route_id?: string;
  /** HTTP method selector for route-aware ops. */
  method?: string;
  sort_by?: string;
  start_line?: number;
  symbols?: string[];
  to?: string;
  to_glob?: string;
  to_sha?: string;
  visibility?: string;
  window_days?: number;
  /** Optional workspace slug for workspace-aware code graph operations. */
  workspace?: string;
}

export interface CodeGraphWorkspace {
  slug: string;
  display?: string;
  root?: string;
  language?: string;
  status?: string;
}

type RouteSelector =
  | { route_id: string; method?: string; path?: string }
  | { route_id?: string; method: string; path: string };

export type RouteMapArgs = Pick<
  CodeGraphArgs,
  "route_id" | "method" | "path_glob" | "framework" | "limit"
>;

export type ShapeCheckArgs = RouteSelector &
  Pick<CodeGraphArgs, "include_optional">;

export type ApiImpactArgs = RouteSelector &
  Pick<CodeGraphArgs, "min_confidence" | "limit">;

export type FlowSearchArgs = Pick<CodeGraphArgs, "limit" | "kind_filter">;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function nonEmptyString(value: unknown): string | undefined {
  return typeof value === "string" && value.trim().length > 0
    ? value.trim()
    : undefined;
}

function normalizeWorkspaceEntry(value: unknown): CodeGraphWorkspace | null {
  if (!isRecord(value)) return null;
  const slug = nonEmptyString(value.slug ?? value.workspace_slug ?? value.id);
  if (!slug) return null;
  return {
    slug,
    display: nonEmptyString(
      value.display ?? value.display_name ?? value.name ?? value.label,
    ),
    root: nonEmptyString(value.root ?? value.root_path ?? value.path),
    language: nonEmptyString(value.language ?? value.indexer),
    status: nonEmptyString(value.status ?? value.warm_status),
  };
}

// ── glqk: index-coverage contract ────────────────────────────────────────────

/** One unindexed (gap) workspace surfaced by the `coverage` op. */
export interface CoverageGapWorkspace {
  slug: string;
  language: string;
  /** indexer_failed | timed_out | unsupported_language. */
  status: string;
  detail?: string;
}

/** Parsed `coverage` op payload — the pieces the galaxy HUD needs. */
export interface CodeGraphCoverage {
  hasGaps: boolean;
  /** Discovered-but-unindexed source roots, one per (workspace, language). */
  gaps: CoverageGapWorkspace[];
}

function normalizeCoverageGap(value: unknown): CoverageGapWorkspace | null {
  if (!isRecord(value)) return null;
  const slug = nonEmptyString(value.workspace_slug ?? value.slug);
  if (!slug) return null;
  return {
    slug,
    language: nonEmptyString(value.language) ?? "unknown",
    status: nonEmptyString(value.status) ?? "indexer_failed",
    detail: nonEmptyString(value.detail),
  };
}

export function parseCoverageResponse(value: unknown): CodeGraphCoverage {
  if (!isRecord(value)) return { hasGaps: false, gaps: [] };
  // Prefer the explicit unindexed_source_roots list; fall back to filtering the
  // full workspaces table on is_gap for forward-compat.
  const rootsRaw = Array.isArray(value.unindexed_source_roots)
    ? value.unindexed_source_roots
    : Array.isArray(value.workspaces)
      ? value.workspaces.filter(
          (w) => isRecord(w) && (w.is_gap === true || w.status === "timed_out"),
        )
      : [];
  const gaps = rootsRaw.flatMap((entry) => {
    const g = normalizeCoverageGap(entry);
    return g ? [g] : [];
  });
  const hasGaps =
    value.has_gaps === true || (value.has_gaps === undefined && gaps.length > 0);
  return { hasGaps, gaps };
}

export function parseWorkspacesResponse(value: unknown): CodeGraphWorkspace[] {
  const candidates = isRecord(value) ? value.workspaces : value;
  if (!Array.isArray(candidates)) return [];
  return candidates.flatMap((entry) => {
    const normalized = normalizeWorkspaceEntry(entry);
    return normalized ? [normalized] : [];
  });
}

/**
 * Generic dispatch — `project` is the slug or UUID, `operation` picks the
 * variant, everything else is forwarded raw and validated server-side.
 */
export async function callCodeGraph(
  project: string,
  operation: CodeGraphOperation,
  args: CodeGraphArgs = {},
): Promise<CodeGraphOutput> {
  return callMcpTool("code_graph", {
    project,
    operation,
    ...args,
  });
}

// ── Per-op wrappers ─────────────────────────────────────────────────────────
// These exist purely as ergonomic shorthands. They do *not* narrow the
// response — that's the parser layer's job (re-exported below).

export function fetchCodeGraphStatus(project: string) {
  return callCodeGraph(project, "status");
}

export function fetchRanked(
  project: string,
  args: Pick<CodeGraphArgs, "limit" | "kind_filter" | "sort_by"> = {},
) {
  return callCodeGraph(project, "ranked", args);
}

/**
 * PR D6: enumerate strongly-connected import cycles. `min_size` defaults
 * to 2 server-side; the FAB chip pre-fills 3 to filter trivial pairs.
 */
export function fetchCycles(
  project: string,
  args: Pick<CodeGraphArgs, "min_size" | "limit"> = {},
) {
  return callCodeGraph(project, "cycles", args);
}

/**
 * PR D6: shortest dependency path between two SCIP keys. Both `from`
 * and `to` are RepoNodeKeys (the same form `selectionId` takes); the
 * server returns `null` under `path` if the nodes are disconnected.
 */
export function fetchPath(
  project: string,
  from: string,
  to: string,
  args: Pick<CodeGraphArgs, "max_depth" | "edge_kind"> = {},
) {
  return callCodeGraph(project, "path", { from, to, ...args });
}

export function searchSymbols(
  project: string,
  query: string,
  args: Pick<CodeGraphArgs, "limit" | "kind_filter" | "kind_hint"> = {},
) {
  return callCodeGraph(project, "search", { query, ...args });
}

export function fetchNeighbors(
  project: string,
  key: string,
  args: Pick<
    CodeGraphArgs,
    "direction" | "kind_filter" | "limit" | "group_by" | "min_confidence"
  > = {},
) {
  return callCodeGraph(project, "neighbors", { key, ...args });
}

export function fetchImpact(
  project: string,
  key: string,
  args: Pick<CodeGraphArgs, "limit" | "group_by" | "min_confidence" | "direction"> = {},
) {
  return callCodeGraph(project, "impact", { key, ...args });
}

export function fetchRouteMap(
  project: string,
  args: RouteMapArgs = {},
) {
  return callCodeGraph(project, "route_map", args);
}

export function fetchShapeCheck(
  project: string,
  args: ShapeCheckArgs,
) {
  return callCodeGraph(project, "shape_check", args);
}

export function fetchApiImpact(
  project: string,
  args: ApiImpactArgs,
) {
  return callCodeGraph(project, "api_impact", args);
}

export function searchFlow(
  project: string,
  query: string,
  args: FlowSearchArgs = {},
) {
  return callCodeGraph(project, "flow", { query, ...args });
}

export const fetchFlow = searchFlow;

export function fetchSymbolsAt(
  project: string,
  file: string,
  start_line: number,
  end_line?: number,
) {
  return callCodeGraph(project, "symbols_at", {
    file,
    start_line,
    ...(end_line !== undefined ? { end_line } : {}),
  });
}

/**
 * PR D2: full-graph snapshot capped by PageRank tier. Drives the
 * `/code-graph` UI render (Sigma + ForceAtlas2). The cap is applied
 * server-side; pass `nodeCap` to override the default of 2000.
 *
 * Pass `level` to request a community-level (`"community"`) snapshot —
 * aggregated nodes representing clusters of symbols — instead of the
 * default symbol-level view. When `level` is omitted the server default
 * (`"symbol"`) applies, so existing callers keep current behavior.
 */
export function fetchSnapshot(
  project: string,
  nodeCap?: number,
  level?: SnapshotLevel,
) {
  return callCodeGraph(project, "snapshot", {
    ...(nodeCap !== undefined ? { limit: nodeCap } : {}),
    ...(level !== undefined ? { level } : {}),
  });
}

export async function fetchWorkspaces(
  project: string,
): Promise<CodeGraphWorkspace[]> {
  const response = await callCodeGraph(project, "workspaces");
  return parseWorkspacesResponse(response);
}

/**
 * glqk: fetch the per-workspace/per-language index-coverage table. Cheap on the
 * server (reads coverage rows, never loads the graph blob). The galaxy HUD uses
 * it to render a coverage-gap banner naming unindexed workspaces.
 */
export async function fetchCoverage(
  project: string,
): Promise<CodeGraphCoverage> {
  const response = await callCodeGraph(project, "coverage");
  return parseCoverageResponse(response);
}

/**
 * PR C1 / D3: 360° symbol view. Pass either a `key` (full RepoNodeKey)
 * or `name` (short name). `include_content` defaults to false because
 * the right-rail Symbol Detail panel renders a header + neighbor list,
 * not the body — D5's chat citations panel will pass `true` to surface
 * the snippet inline.
 */
export function fetchContext(
  project: string,
  args: { key?: string; name?: string; include_content?: boolean },
) {
  return callCodeGraph(project, "context", args);
}

/**
 * PR B4 / D3: hybrid (RRF-fused) symbol search. `mode` defaults to
 * server-side `hybrid` when omitted; the Cmd-K palette pins it
 * explicitly so behavior doesn't drift if the env default changes.
 */
export function searchHybrid(
  project: string,
  query: string,
  args: Pick<CodeGraphArgs, "limit" | "kind_filter" | "kind_hint"> = {},
) {
  return callCodeGraph(project, "search", {
    query,
    mode: "hybrid",
    ...args,
  });
}

// ── Re-export the existing untagged-response parsers ───────────────────────
// `pulseTypes` lives in `components/pulse/` for historic reasons. Once D2+
// land we can move the file under `api/` if the layout still feels off.

export {
  asArray,
  fileFromKey,
  parseAmbiguous,
  parseCycles,
  parseDetectedChanges,
  parseFileGroups,
  parseImpactDetailed,
  parseNeighbors,
  parseNotFound,
  parseOrphans,
  parsePath,
  parseRanked,
  parseSearchHits,
  parseSymbolContext,
  truncatePathLeft,
  type Candidate,
  type ChangeKind,
  type CycleGroup,
  type CycleMember,
  type DetectedChangesResult,
  type DetectedTouchedSymbol,
  type EdgeCategory,
  type FileGroupEntry,
  type GraphNeighbor,
  type MethodMeta,
  type MethodParam,
  type NotFound,
  type OrphanEntry,
  type PagerankTier,
  type PathHop,
  type PathResult,
  type ProcessRef,
  type RankedNode,
  type RelatedSymbol,
  type SearchHit,
  type SymbolContext,
  type SymbolNode,
} from "@/components/pulse/pulseTypes";
