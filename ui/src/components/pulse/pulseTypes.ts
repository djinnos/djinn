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
