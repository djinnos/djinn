// Shared client-side narrowing for the untyped `code_graph` MCP responses.
// The server returns `Record<string, unknown>` to the desktop, so each panel
// validates the shape it cares about with these helpers.

export interface RankedNode {
  key: string;
  kind: string;
  display_name: string;
  score: number;
  page_rank: number;
  structural_weight: number;
  inbound_edge_weight: number;
  outbound_edge_weight: number;
}

export interface OrphanEntry {
  key: string;
  kind: string;
  display_name: string;
  file: string | null;
  visibility: string;
}

export interface CycleMember {
  key: string;
  display_name: string;
  kind: string;
}

export interface CycleGroup {
  size: number;
  members: CycleMember[];
}

export interface SearchHit {
  key: string;
  kind: string;
  display_name: string;
  score: number;
  file: string | null;
}

export interface FileGroupEntry {
  file: string;
  occurrence_count: number;
  max_depth: number;
  sample_keys: string[];
}

export interface GraphNeighbor {
  key: string;
  kind: string;
  display_name: string;
  edge_kind: string;
  edge_weight: number;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === "object";
}

export function asArray(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

/**
 * The Rust `CodeGraphResponse` enum is `#[serde(untagged)]`, so every `code_graph`
 * operation returns the inner response struct directly on the wire:
 *
 *   ranked    → { nodes:     [...] }
 *   orphans   → { orphans:   [...] }
 *   cycles    → { cycles:    [...] }
 *   search    → { hits:      [...] }
 *   neighbors → { neighbors: [...] }
 *   impact    → { file_groups: [...] }  (or similar — depends on group_by)
 *
 * Every parser below is handed that wrapper.  This helper pulls the named
 * field, tolerates the already-unwrapped array form (tests, call sites that
 * slice manually), and falls back to `[]` for anything else.
 */
function unwrapList(value: unknown, field: string): unknown[] {
  if (Array.isArray(value)) return value;
  if (isRecord(value)) {
    const inner = (value as Record<string, unknown>)[field];
    if (Array.isArray(inner)) return inner;
  }
  return [];
}

export function parseRanked(value: unknown): RankedNode[] {
  return unwrapList(value, "nodes")
    .filter(isRecord)
    .map((r) => ({
      key: String(r.key ?? ""),
      kind: String(r.kind ?? ""),
      display_name: String(r.display_name ?? ""),
      score: Number(r.score ?? 0),
      page_rank: Number(r.page_rank ?? 0),
      structural_weight: Number(r.structural_weight ?? 0),
      inbound_edge_weight: Number(r.inbound_edge_weight ?? 0),
      outbound_edge_weight: Number(r.outbound_edge_weight ?? 0),
    }))
    .filter((r) => r.key.length > 0);
}

export function parseOrphans(value: unknown): OrphanEntry[] {
  return unwrapList(value, "orphans")
    .filter(isRecord)
    .map((r) => ({
      key: String(r.key ?? ""),
      kind: String(r.kind ?? ""),
      display_name: String(r.display_name ?? ""),
      file: typeof r.file === "string" ? r.file : null,
      visibility: String(r.visibility ?? "unknown"),
    }))
    .filter((r) => r.key.length > 0);
}

export function parseCycles(value: unknown): CycleGroup[] {
  return unwrapList(value, "cycles")
    .filter(isRecord)
    .map((r) => {
      const members = asArray(r.members)
        .filter(isRecord)
        .map((m) => ({
          key: String(m.key ?? ""),
          display_name: String(m.display_name ?? ""),
          kind: String(m.kind ?? ""),
        }));
      return {
        size: Number(r.size ?? members.length),
        members,
      };
    })
    .filter((g) => g.members.length > 0);
}

export function parseSearchHits(value: unknown): SearchHit[] {
  return unwrapList(value, "hits")
    .filter(isRecord)
    .map((r) => ({
      key: String(r.key ?? ""),
      kind: String(r.kind ?? ""),
      display_name: String(r.display_name ?? ""),
      score: Number(r.score ?? 0),
      file: typeof r.file === "string" ? r.file : null,
    }))
    .filter((r) => r.key.length > 0);
}

export function parseFileGroups(value: unknown): FileGroupEntry[] {
  return unwrapList(value, "file_groups")
    .filter(isRecord)
    .map((r) => ({
      file: String(r.file ?? ""),
      occurrence_count: Number(r.occurrence_count ?? 0),
      max_depth: Number(r.max_depth ?? 0),
      sample_keys: asArray(r.sample_keys).map(String),
    }))
    .filter((r) => r.file.length > 0);
}

export function parseNeighbors(value: unknown): GraphNeighbor[] {
  return unwrapList(value, "neighbors")
    .filter(isRecord)
    .map((r) => ({
      key: String(r.key ?? ""),
      kind: String(r.kind ?? ""),
      display_name: String(r.display_name ?? ""),
      edge_kind: String(r.edge_kind ?? ""),
      edge_weight: Number(r.edge_weight ?? 0),
    }))
    .filter((r) => r.key.length > 0);
}

// ── detect_changes (PR C4) ──────────────────────────────────────────────────

export type PagerankTier = "high" | "medium" | "low";
export type ChangeKind = "added" | "modified" | "deleted";

export interface DetectedTouchedSymbol {
  uid: string;
  name: string;
  kind: string;
  file_path: string;
  start_line: number;
  end_line: number;
  pagerank_tier: PagerankTier;
  change_kind: ChangeKind;
}

export interface DetectedChangesResult {
  from_sha: string;
  to_sha: string;
  touched_symbols: DetectedTouchedSymbol[];
  by_file: Record<string, DetectedTouchedSymbol[]>;
}

function asPagerankTier(value: unknown): PagerankTier {
  return value === "high" || value === "medium" || value === "low"
    ? value
    : "low";
}

function asChangeKind(value: unknown): ChangeKind {
  return value === "added" || value === "modified" || value === "deleted"
    ? value
    : "modified";
}

function parseDetectedTouchedSymbol(value: unknown): DetectedTouchedSymbol | null {
  if (!isRecord(value)) return null;
  const uid = String(value.uid ?? "");
  if (uid.length === 0) return null;
  return {
    uid,
    name: String(value.name ?? ""),
    kind: String(value.kind ?? ""),
    file_path: String(value.file_path ?? ""),
    start_line: Number(value.start_line ?? 0),
    end_line: Number(value.end_line ?? 0),
    pagerank_tier: asPagerankTier(value.pagerank_tier),
    change_kind: asChangeKind(value.change_kind),
  };
}

/**
 * Narrow a `code_graph detect_changes` response. The discriminator
 * field (per the inter-PR contract) is `detected_changes`, an object
 * shaped `{from_sha, to_sha, touched_symbols, by_file}`.
 *
 * Returns `null` when the response is for a different `code_graph`
 * variant — callers can chain a `?? defaultValue` for graceful
 * fallback.
 */
export function parseDetectedChanges(value: unknown): DetectedChangesResult | null {
  if (!isRecord(value)) return null;
  const detected = (value as Record<string, unknown>).detected_changes;
  if (!isRecord(detected)) return null;

  const touchedRaw = asArray(detected.touched_symbols);
  const touched_symbols = touchedRaw
    .map(parseDetectedTouchedSymbol)
    .filter((s): s is DetectedTouchedSymbol => s !== null);

  const by_file: Record<string, DetectedTouchedSymbol[]> = {};
  if (isRecord(detected.by_file)) {
    for (const [file, list] of Object.entries(
      detected.by_file as Record<string, unknown>,
    )) {
      const items = asArray(list)
        .map(parseDetectedTouchedSymbol)
        .filter((s): s is DetectedTouchedSymbol => s !== null);
      by_file[file] = items;
    }
  }

  return {
    from_sha: String(detected.from_sha ?? ""),
    to_sha: String(detected.to_sha ?? ""),
    touched_symbols,
    by_file,
  };
}

// ── Display helpers ─────────────────────────────────────────────────────────

/** Truncate a long path from the left: `/a/b/c/d.rs` → `…/c/d.rs`. */
export function truncatePathLeft(path: string, maxLen = 56): string {
  if (path.length <= maxLen) return path;
  const tail = path.slice(path.length - maxLen + 1);
  const slash = tail.indexOf("/");
  return "…" + (slash >= 0 ? tail.slice(slash) : tail);
}

/** Heuristic: extract a file path from a node key (file keys are bare paths). */
export function fileFromKey(key: string, fallbackFile: string | null): string {
  if (fallbackFile) return fallbackFile;
  // SCIP symbol keys typically include `\u0020` or `:` and `#`/`/` separators.
  // For our purposes the safe assumption is: if the key looks like a path
  // (contains `/` and a recognizable extension), return it; otherwise empty.
  if (/^[\w./@-]+\.[a-z]+$/i.test(key)) return key;
  return "";
}
