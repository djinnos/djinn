import { normalizeKind } from "@/lib/codeGraphAdapter";
import type { SnapshotEdge, SnapshotNode, SnapshotPayload } from "@/lib/codeGraphAdapter";

export const GALAXY_ARTIFACT_VERSION = 1;
export const IDENTITY_HEADERS = {
  projectId: "x-djinn-project-id",
  generationId: "x-djinn-generation-id",
  commitSha: "x-djinn-commit-sha",
  artifactVersion: "x-djinn-galaxy-artifact-version",
  semanticHash: "x-djinn-galaxy-semantic-sha256",
} as const;

export interface GalaxyArtifactIdentity {
  projectId: string;
  generationId: string;
  commitSha: string;
  artifactVersion: number;
  semanticHash: string;
  etag: string;
}

export interface ValidatedGalaxyArtifact extends GalaxyArtifactIdentity {
  snapshot: SnapshotPayload;
}

function fail(message: string): never {
  throw new Error(`Galaxy artifact validation failed: ${message}`);
}

function record(value: unknown, name: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) fail(`${name} is not an object`);
  return value as Record<string, unknown>;
}
function string(value: unknown, name: string): string {
  if (typeof value !== "string" || value.length === 0) fail(`${name} is missing or invalid`);
  return value;
}
// Content string: type-guarded only, value-tolerant. Reserved for node/edge
// display fields (label, keywords, symbol_kind, file_path, community_id,
// workspace, reason, …) whose producer counterparts are unconstrained `String`
// / `Option<String>` and can legitimately be empty (e.g. a synthetic Table /
// Process node whose `display_name` prettifies to ""). The renderer's adapter
// (`codeGraphAdapter`) already tolerates empty here, so hard-failing on empty
// would make this validator stricter than both the producer and the consumer
// it feeds. Identity/structure strings (ids, hashes, headers) keep `string`.
function contentString(value: unknown, name: string): string {
  if (typeof value !== "string") fail(`${name} is not a string`);
  return value;
}
function finite(value: unknown, name: string): number {
  if (typeof value !== "number" || !Number.isFinite(value)) fail(`${name} is not finite`);
  return value;
}
function integer(value: unknown, name: string): number {
  const number = finite(value, name);
  if (!Number.isInteger(number) || number < 0) fail(`${name} is not a non-negative integer`);
  return number;
}
// Optional content string: type-guarded, value-tolerant (empty allowed). Only
// used for optional node/edge display fields, never for identity — see
// `contentString`.
function optionalString(value: unknown, name: string): string | undefined {
  if (value === undefined) return undefined;
  return contentString(value, name);
}
function optionalFinite(value: unknown, name: string): number | undefined {
  if (value === undefined) return undefined;
  return finite(value, name);
}

function node(value: unknown, index: number): SnapshotNode {
  const raw = record(value, `nodes[${index}]`);
  // Validate `kind` as a present, non-empty string, then normalize it exactly
  // the way the renderer's adapter does — the producer emits synthetic kinds
  // ("process", "table", "route", "tool") beyond file/folder/symbol/community,
  // and `normalizeKind` folds anything outside the renderable set to "symbol".
  // Enumerating the set here made the validator stricter than both the producer
  // and the adapter it feeds. Safe post-hash: the semantic hash is computed over
  // the raw producer bytes (see `canonicalSemanticJson`), so normalizing values
  // afterward cannot affect integrity verification.
  const kind = normalizeKind(string(raw.kind, `nodes[${index}].kind`));
  const keywords = raw.keywords === undefined ? undefined : Array.isArray(raw.keywords)
    ? raw.keywords.map((keyword, keywordIndex) => contentString(keyword, `nodes[${index}].keywords[${keywordIndex}]`))
    : fail(`nodes[${index}].keywords is not an array`);
  return {
    id: string(raw.id, `nodes[${index}].id`), kind, label: contentString(raw.label, `nodes[${index}].label`),
    symbol_kind: optionalString(raw.symbol_kind, `nodes[${index}].symbol_kind`),
    file_path: optionalString(raw.file_path, `nodes[${index}].file_path`),
    pagerank: finite(raw.pagerank, `nodes[${index}].pagerank`),
    community_id: optionalString(raw.community_id, `nodes[${index}].community_id`), keywords,
    member_count: raw.member_count === undefined ? undefined : integer(raw.member_count, `nodes[${index}].member_count`),
    internal_edge_count: raw.internal_edge_count === undefined ? undefined : integer(raw.internal_edge_count, `nodes[${index}].internal_edge_count`),
    workspace_kind: optionalString(raw.workspace_kind, `nodes[${index}].workspace_kind`), workspace: optionalString(raw.workspace, `nodes[${index}].workspace`),
    workspace_context: raw.workspace_context === undefined ? undefined : raw.workspace_context === true,
    cognitive: optionalFinite(raw.cognitive, `nodes[${index}].cognitive`), is_test: raw.is_test === undefined ? undefined : raw.is_test === true,
    x: optionalFinite(raw.x, `nodes[${index}].x`), y: optionalFinite(raw.y, `nodes[${index}].y`), gx: optionalFinite(raw.gx, `nodes[${index}].gx`), gy: optionalFinite(raw.gy, `nodes[${index}].gy`), gz: optionalFinite(raw.gz, `nodes[${index}].gz`),
    degree: raw.degree === undefined ? undefined : integer(raw.degree, `nodes[${index}].degree`),
  };
}
function edge(value: unknown, index: number): SnapshotEdge {
  const raw = record(value, `edges[${index}]`);
  return { from: string(raw.from, `edges[${index}].from`), to: string(raw.to, `edges[${index}].to`), kind: string(raw.kind, `edges[${index}].kind`), confidence: finite(raw.confidence, `edges[${index}].confidence`), reason: optionalString(raw.reason, `edges[${index}].reason`) };
}

/** Browser-compatible SHA-256 hex, deliberately independent of server output. */
export async function sha256Hex(bytes: BufferSource): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

/**
 * The producer's canonical semantic domain is the payload's own decoded BYTES
 * with only the `graph_content_hash` field spliced out — deliberately not a JS
 * re-serialization. serde_json/ryu formats an integral f64 as `0.0`/`1.0`,
 * which is not a fixed point of `JSON.parse`→`JSON.stringify` (`0`/`1`), so
 * re-stringifying would diverge from the producer bytes for real artifacts.
 *
 * The producer serializes the same struct twice: once with `graph_content_hash`
 * absent (the hash input) and once with it present (the final payload). Because
 * the field is written in a fixed serde position (immediately after
 * `generation_id`, immediately before `truncated`), the final payload minus the
 * literal `"graph_content_hash":"<hex>",` substring is byte-identical to the
 * hash input. `payloadHash` has already been validated as 64 lowercase hex, so
 * the literal is exact; it must occur exactly once or we refuse to guess.
 */
export function canonicalSemanticJson(payloadText: string, payloadHash: string): string {
  const literal = `"graph_content_hash":"${payloadHash}",`;
  const first = payloadText.indexOf(literal);
  if (first === -1 || payloadText.indexOf(literal, first + 1) !== -1) {
    fail("semantic hash field is not uniquely spliceable");
  }
  return payloadText.slice(0, first) + payloadText.slice(first + literal.length);
}

export async function validateGalaxyArtifact(
  requestedProjectId: string,
  headers: Headers,
  payload: unknown,
  /** Exact decompressed payload bytes as text, the producer's own serialization. */
  payloadText: string,
  /** Exact compressed HTTP representation, before gzip decoding. */
  transportBytes: BufferSource,
): Promise<ValidatedGalaxyArtifact> {
  const raw = record(payload, "payload");
  const identity: GalaxyArtifactIdentity = {
    projectId: string(headers.get(IDENTITY_HEADERS.projectId), IDENTITY_HEADERS.projectId),
    generationId: string(headers.get(IDENTITY_HEADERS.generationId), IDENTITY_HEADERS.generationId),
    commitSha: string(headers.get(IDENTITY_HEADERS.commitSha), IDENTITY_HEADERS.commitSha),
    artifactVersion: Number(string(headers.get(IDENTITY_HEADERS.artifactVersion), IDENTITY_HEADERS.artifactVersion)),
    semanticHash: string(headers.get(IDENTITY_HEADERS.semanticHash), IDENTITY_HEADERS.semanticHash),
    etag: string(headers.get("etag"), "etag"),
  };
  if (!/^"[0-9a-f]{64}"$/.test(identity.etag)) fail("etag is not a strong SHA-256 entity tag");
  // The ETag authenticates the compressed transport representation, not just
  // an arbitrary label attached to valid decoded JSON.
  const transportHash = await sha256Hex(transportBytes);
  if (identity.etag !== `"${transportHash}"`) fail("transport etag recomputation mismatch");
  if (!Number.isInteger(identity.artifactVersion) || identity.artifactVersion !== GALAXY_ARTIFACT_VERSION) fail("artifact version is unsupported");
  const payloadProject = string(raw.project_id, "payload.project_id");
  const payloadGeneration = string(raw.generation_id, "payload.generation_id");
  const payloadCommit = string(raw.git_head, "payload.git_head");
  const payloadHash = string(raw.graph_content_hash, "payload.graph_content_hash");
  if (requestedProjectId !== identity.projectId || payloadProject !== requestedProjectId || payloadProject !== identity.projectId) fail("project identity mismatch");
  if (payloadGeneration !== identity.generationId) fail("generation identity mismatch");
  if (payloadCommit !== identity.commitSha) fail("commit identity mismatch");
  if (payloadHash !== identity.semanticHash || !/^[0-9a-f]{64}$/.test(payloadHash)) fail("semantic hash identity mismatch");
  const computedHash = await sha256Hex(new TextEncoder().encode(canonicalSemanticJson(payloadText, payloadHash)));
  if (computedHash !== payloadHash) fail("semantic hash recomputation mismatch");
  if (!Array.isArray(raw.nodes) || !Array.isArray(raw.edges)) fail("nodes or edges is not an array");
  const nodes = raw.nodes.map(node); const edges = raw.edges.map(edge);
  const snapshot: SnapshotPayload = {
    project_id: payloadProject, git_head: payloadCommit, generated_at: string(raw.generated_at, "payload.generated_at"),
    truncated: raw.truncated === true, total_nodes: integer(raw.total_nodes, "payload.total_nodes"), total_edges: integer(raw.total_edges, "payload.total_edges"), node_cap: integer(raw.node_cap, "payload.node_cap"), nodes, edges,
  };
  if (snapshot.total_nodes !== nodes.length || snapshot.total_edges !== edges.length || snapshot.node_cap < nodes.length) fail("payload counts do not match contents");
  return { ...identity, snapshot };
}
