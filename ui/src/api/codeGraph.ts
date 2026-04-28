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
  | "snapshot";

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
  changed_ranges?: Array<{ file: string; start_line: number; end_line?: number }>;
  confidence?: string;
  direction?: string;
  edge_kind?: string;
  end_line?: number;
  file?: string;
  file_glob?: string;
  from?: string;
  from_glob?: string;
  from_sha?: string;
  group_by?: string;
  /** PR C1 `context` op: fetch the symbol body verbatim. Defaults false. */
  include_content?: boolean;
  key?: string;
  kind_filter?: string;
  kind_hint?: string;
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
  query?: string;
  sort_by?: string;
  start_line?: number;
  symbols?: string[];
  to?: string;
  to_glob?: string;
  to_sha?: string;
  visibility?: string;
  window_days?: number;
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
  args: Pick<CodeGraphArgs, "limit" | "group_by" | "min_confidence"> = {},
) {
  return callCodeGraph(project, "impact", { key, ...args });
}

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
 */
export function fetchSnapshot(project: string, nodeCap?: number) {
  return callCodeGraph(project, "snapshot", {
    ...(nodeCap !== undefined ? { limit: nodeCap } : {}),
  });
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
  parseNeighbors,
  parseNotFound,
  parseOrphans,
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
  type ProcessRef,
  type RankedNode,
  type RelatedSymbol,
  type SearchHit,
  type SymbolContext,
  type SymbolNode,
} from "@/components/pulse/pulseTypes";
