/**
 * impactMermaid — pure builders for the PR D4 ImpactFlowModal.
 *
 * Kept in its own module so the modal file can stay component-only (the
 * `react-refresh/only-export-components` lint rule complains otherwise),
 * and so unit tests can pin the exact Mermaid string output without
 * pulling in a JSX render path.
 */

/**
 * SCREAMING_SNAKE_CASE bucket emitted by the server (see PR C3,
 * `ImpactRisk` in `djinn-control-plane`). Mirrored on the client so we can
 * type the modal props without a generated wrapper.
 */
export type ImpactRisk = "LOW" | "MEDIUM" | "HIGH" | "CRITICAL";

/**
 * Impact entry on the wire — see `ImpactEntry` in
 * `server/crates/djinn-control-plane/src/bridge.rs`.
 */
export interface ImpactEntry {
  key: string;
  depth: number;
  /**
   * Optional human-readable name for the entry. Server doesn't surface
   * `display_name` for impact today (the Rust struct has only `key` +
   * `depth`); we keep this field on the client so a future server change
   * — or a parent component that already has neighbor metadata — can
   * pass nicer labels.
   */
  display_name?: string;
}

/**
 * Detailed `code_graph impact` result with PR C3 fields. The server
 * splits the wire shape into "detailed" vs "grouped" depending on
 * `group_by`; this modal renders the detailed branch only — file
 * rollups don't fit the depth-bucket flowchart story.
 */
export interface ImpactDetailedResult {
  /** Queried node key — drawn as the central node in the flowchart. */
  key: string;
  /** Display label for the queried node; falls back to a trimmed `key`. */
  target_label?: string;
  entries: ImpactEntry[];
  risk?: ImpactRisk | null;
  summary?: string | null;
}

/**
 * Build a Mermaid `flowchart TD` from a detailed impact result.
 *
 * Layout strategy:
 *   - One central node for the queried symbol.
 *   - One subgraph per depth bucket (`Direct (depth 1)`, `Depth 2`, …),
 *     containing every entry whose depth equals that bucket.
 *   - Edges: depth-1 entries point at the target; deeper entries point at
 *     the *closest* shallower entry we can pick. We don't have the actual
 *     edge list from the server (`impact` only returns `{key, depth}`), so
 *     the chain is a best-effort visual — each entry is drawn as
 *     `entry --> firstEntryAtPriorDepth`. This still communicates "this
 *     function is two hops from the target" without faking edges that
 *     might not exist in the underlying graph.
 *
 * The function is pure and exported so a unit test can pin the exact
 * output across plan iterations.
 */
export function buildImpactMermaid(impact: ImpactDetailedResult): string {
  const lines: string[] = ["flowchart TD"];
  const targetId = "target";
  const targetLabel = impact.target_label ?? trimKey(impact.key);
  lines.push(`  ${targetId}["${escapeLabel(targetLabel)}"]:::target`);

  if (impact.entries.length === 0) {
    lines.push("classDef target fill:#fde68a,stroke:#b45309;");
    return lines.join("\n");
  }

  // Bucket entries by depth, sorting depths ascending. Empty depths are
  // skipped — the server may emit gaps if `min_confidence` filters out
  // intermediate nodes.
  const byDepth = new Map<number, Array<ImpactEntry & { id: string }>>();
  impact.entries.forEach((entry, index) => {
    const id = `n${index}`;
    const depth = entry.depth;
    if (!byDepth.has(depth)) byDepth.set(depth, []);
    byDepth.get(depth)!.push({ ...entry, id });
  });

  const depths = [...byDepth.keys()].sort((a, b) => a - b);

  for (const depth of depths) {
    const bucket = byDepth.get(depth)!;
    const subgraphLabel = depth === 1 ? "Direct (depth 1)" : `Depth ${depth}`;
    const subgraphId = `depth_${depth}`;
    lines.push(`  subgraph ${subgraphId}["${escapeLabel(subgraphLabel)}"]`);
    for (const entry of bucket) {
      const label = entry.display_name ?? trimKey(entry.key);
      lines.push(`    ${entry.id}["${escapeLabel(label)}"]`);
    }
    lines.push("  end");
  }

  // Edges. Depth-1 entries point at the target. Deeper buckets point at the
  // first entry of the immediately shallower bucket — see the docstring for
  // why this is a "best-effort" chain rather than ground truth.
  const firstAtDepth = new Map<number, string>();
  for (const depth of depths) {
    const bucket = byDepth.get(depth)!;
    firstAtDepth.set(depth, bucket[0]!.id);
  }

  for (const depth of depths) {
    const bucket = byDepth.get(depth)!;
    const parentId =
      depth === 1
        ? targetId
        : firstAtDepth.get(closestShallowerDepth(depths, depth)) ?? targetId;
    for (const entry of bucket) {
      lines.push(`  ${entry.id} --> ${parentId}`);
    }
  }

  // Style the target so the eye snaps to it.
  lines.push("  classDef target fill:#fde68a,stroke:#b45309;");
  return lines.join("\n");
}

function closestShallowerDepth(depths: number[], depth: number): number {
  // Walk the sorted depth list looking for the largest entry strictly less
  // than `depth`. Caller guarantees `depth` is in the list.
  let best = depths[0]!;
  for (const d of depths) {
    if (d < depth) best = d;
    else break;
  }
  return best;
}

/**
 * Escape characters that break Mermaid label parsing. Quoted-label syntax
 * (`["Foo Bar"]`) supports most characters except literal `"` — we strip
 * those defensively. Backslashes get doubled to keep the label readable.
 */
function escapeLabel(text: string): string {
  return text.replace(/\\/g, "\\\\").replace(/"/g, "'");
}

/**
 * Trim a SCIP key down to a readable tail. SCIP keys look like
 * `local . my_crate v1 mod/file.rs#fn_name()`; the rendered label only
 * needs the trailing identifier so the modal stays scannable.
 */
function trimKey(key: string): string {
  // Common SCIP separator: ` ` between scheme/version/path. The last
  // path segment after a `/` or `#`/`.` is usually the human name.
  const lastSpace = key.lastIndexOf(" ");
  const tail = lastSpace >= 0 ? key.slice(lastSpace + 1) : key;
  // Then, prefer everything after the final `#` or `/` for the symbol.
  const hash = tail.lastIndexOf("#");
  if (hash >= 0 && hash < tail.length - 1) return tail.slice(hash + 1);
  const slash = tail.lastIndexOf("/");
  if (slash >= 0 && slash < tail.length - 1) return tail.slice(slash + 1);
  return tail;
}
