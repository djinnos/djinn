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
  /**
   * PR B4: tags the signal that surfaced this hit when the caller
   * asked for `mode=hybrid`. One of `"lexical"`, `"semantic"`,
   * `"structural"`, or `"hybrid"` (when RRF fused the hit across
   * multiple signals). `null` for the legacy `mode=name` fast path —
   * old client builds that don't read this field stay backwards-
   * compatible because the server omits it via
   * `skip_serializing_if = "Option::is_none"`.
   */
  match_kind: string | null;
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

/**
 * PR C2 disambiguation candidate emitted by `code_graph` when a
 * short-name lookup (`User`, `helper`) hits more than one node. The
 * `uid` is a stable RepoNodeKey — pass it back as `key` for an
 * unambiguous follow-up.
 */
export interface Candidate {
  uid: string;
  name: string;
  kind: string;
  file_path: string;
  score: number;
}

/** PR C2: structured "no match" body. The `not_found` object disambiguates
 *  this variant from every other `code_graph` response. */
export interface NotFound {
  query: string;
  kind_hint: string | null;
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
      // PR B4: hybrid-mode hits carry a `match_kind` tag for debug
      // surfaces. Absent on legacy `mode=name` responses → null.
      match_kind:
        typeof r.match_kind === "string" ? r.match_kind : null,
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

/**
 * PR C2: parse the `Ambiguous` variant.  Discriminator field is
 * `candidates` — every other `CodeGraphResponse` variant uses a
 * different top-level field name, so a value carrying `candidates` is
 * unambiguously this branch.  Returns `[]` when the value isn't an
 * `Ambiguous` payload.
 */
export function parseAmbiguous(value: unknown): Candidate[] {
  return unwrapList(value, "candidates")
    .filter(isRecord)
    .map((r) => ({
      uid: String(r.uid ?? ""),
      name: String(r.name ?? ""),
      kind: String(r.kind ?? ""),
      file_path: String(r.file_path ?? ""),
      score: Number(r.score ?? 0),
    }))
    .filter((c) => c.uid.length > 0);
}

/**
 * PR C2: parse the `NotFound` variant.  Discriminator field is
 * `not_found` (an object with `{query, kind_hint?}`).  Returns `null`
 * when the value isn't a `NotFound` payload.
 */
export function parseNotFound(value: unknown): NotFound | null {
  if (!isRecord(value)) return null;
  const inner = (value as Record<string, unknown>)["not_found"];
  if (!isRecord(inner)) return null;
  const query = inner.query;
  if (typeof query !== "string") return null;
  return {
    query,
    kind_hint:
      typeof inner.kind_hint === "string" ? inner.kind_hint : null,
  };
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

// ── context (PR C1) ─────────────────────────────────────────────────────────

/**
 * PR C1: edge categories used to bucket incoming/outgoing neighbors in
 * the `context` op. Mirrors the Rust `EdgeCategory` enum verbatim and
 * the inter-PR contract table mapping `RepoGraphEdgeKind` → category.
 */
export type EdgeCategory =
  | "calls"
  | "references"
  | "imports"
  | "contains"
  | "extends"
  | "implements"
  | "type_defines"
  | "defines"
  | "reads"
  | "writes";

/** PR C1: a neighbor of the queried symbol, grouped under its EdgeCategory. */
export interface RelatedSymbol {
  uid: string;
  name: string;
  kind: string;
  file_path: string | null;
  confidence: number;
}

/** PR C1: a single structured method parameter. */
export interface MethodParam {
  name: string;
  type_name: string | null;
  default_value: string | null;
}

/** PR C1: structured method metadata. Populated only when SCIP emits
 * structured signature fields; `null` when the indexer only emits the
 * markdown signature blob. */
export interface MethodMeta {
  visibility: string | null;
  is_async: boolean | null;
  params: MethodParam[];
  return_type: string | null;
  annotations: string[];
}

/** PR C1: F2 stub. The list is empty until process membership lands. */
export interface ProcessRef {
  id: string;
  label: string;
  role: string;
}

/** PR C1: identity + structural metadata of the queried symbol. */
export interface SymbolNode {
  uid: string;
  name: string;
  kind: string;
  file_path: string;
  start_line: number;
  end_line: number;
  content: string | null;
  method_metadata: MethodMeta | null;
}

/** PR C1: 360° symbol view returned by `code_graph context`. */
export interface SymbolContext {
  symbol: SymbolNode;
  incoming: Partial<Record<EdgeCategory, RelatedSymbol[]>>;
  outgoing: Partial<Record<EdgeCategory, RelatedSymbol[]>>;
  processes: ProcessRef[];
}

const EDGE_CATEGORIES: readonly EdgeCategory[] = [
  "calls",
  "references",
  "imports",
  "contains",
  "extends",
  "implements",
  "type_defines",
  "defines",
  "reads",
  "writes",
] as const;

function asEdgeCategory(value: unknown): EdgeCategory | null {
  return typeof value === "string" &&
    (EDGE_CATEGORIES as readonly string[]).includes(value)
    ? (value as EdgeCategory)
    : null;
}

function parseRelatedSymbol(value: unknown): RelatedSymbol | null {
  if (!isRecord(value)) return null;
  const uid = String(value.uid ?? "");
  if (uid.length === 0) return null;
  return {
    uid,
    name: String(value.name ?? ""),
    kind: String(value.kind ?? ""),
    file_path: typeof value.file_path === "string" ? value.file_path : null,
    confidence: Number(value.confidence ?? 0),
  };
}

function parseRelatedMap(
  value: unknown,
): Partial<Record<EdgeCategory, RelatedSymbol[]>> {
  if (!isRecord(value)) return {};
  const out: Partial<Record<EdgeCategory, RelatedSymbol[]>> = {};
  for (const [rawKey, rawList] of Object.entries(value)) {
    const cat = asEdgeCategory(rawKey);
    if (!cat) continue;
    const items = asArray(rawList)
      .map(parseRelatedSymbol)
      .filter((r): r is RelatedSymbol => r !== null);
    out[cat] = items;
  }
  return out;
}

function parseMethodParam(value: unknown): MethodParam | null {
  if (!isRecord(value)) return null;
  const name = String(value.name ?? "");
  if (name.length === 0) return null;
  return {
    name,
    type_name: typeof value.type_name === "string" ? value.type_name : null,
    default_value:
      typeof value.default_value === "string" ? value.default_value : null,
  };
}

function parseMethodMeta(value: unknown): MethodMeta | null {
  if (!isRecord(value)) return null;
  return {
    visibility: typeof value.visibility === "string" ? value.visibility : null,
    is_async: typeof value.is_async === "boolean" ? value.is_async : null,
    params: asArray(value.params)
      .map(parseMethodParam)
      .filter((p): p is MethodParam => p !== null),
    return_type:
      typeof value.return_type === "string" ? value.return_type : null,
    annotations: asArray(value.annotations).map(String),
  };
}

function parseProcessRef(value: unknown): ProcessRef | null {
  if (!isRecord(value)) return null;
  const id = String(value.id ?? "");
  if (id.length === 0) return null;
  return {
    id,
    label: String(value.label ?? ""),
    role: String(value.role ?? ""),
  };
}

function parseSymbolNode(value: unknown): SymbolNode | null {
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
    content: typeof value.content === "string" ? value.content : null,
    method_metadata: parseMethodMeta(value.method_metadata),
  };
}

/**
 * Narrow a `code_graph context` response. The discriminator field per
 * the inter-PR contract is `symbol_context`, an object shaped
 * `{symbol, incoming, outgoing, processes}`. Returns `null` when the
 * value is for a different `code_graph` variant.
 */
export function parseSymbolContext(value: unknown): SymbolContext | null {
  if (!isRecord(value)) return null;
  const inner = (value as Record<string, unknown>).symbol_context;
  if (!isRecord(inner)) return null;
  const symbol = parseSymbolNode(inner.symbol);
  if (!symbol) return null;
  const incoming = parseRelatedMap(inner.incoming);
  const outgoing = parseRelatedMap(inner.outgoing);
  const processes = asArray(inner.processes)
    .map(parseProcessRef)
    .filter((p): p is ProcessRef => p !== null);
  return { symbol, incoming, outgoing, processes };
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
