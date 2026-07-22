import { getServerBaseUrl } from "@/api/serverUrl";
import {
  IDENTITY_HEADERS,
  type ValidatedGalaxyArtifact,
  validateGalaxyArtifact,
} from "@/api/galaxyArtifactSchema";

export type GalaxyArtifactOutcome =
  | { kind: "artifact"; artifact: ValidatedGalaxyArtifact }
  | { kind: "fallback"; reason: "unavailable" | "unsupported" };

// Retain validated rollback representations as well as the newest one. A
// multi-value If-None-Match lets the server select an older cached ETag.
const cache = new Map<string, Map<string, ValidatedGalaxyArtifact>>();
const UNAVAILABLE = "galaxy_artifact_unavailable";
const UNSUPPORTED = "galaxy_artifact_unsupported";

function failure(message: string): never {
  throw new Error(`Galaxy artifact request failed: ${message}`);
}

function cacheMatchesRequest(projectId: string, artifact: ValidatedGalaxyArtifact, headers: Headers): boolean {
  return artifact.projectId === projectId &&
    artifact.projectId === headers.get(IDENTITY_HEADERS.projectId) &&
    artifact.generationId === headers.get(IDENTITY_HEADERS.generationId) &&
    artifact.commitSha === headers.get(IDENTITY_HEADERS.commitSha) &&
    String(artifact.artifactVersion) === headers.get(IDENTITY_HEADERS.artifactVersion) &&
    artifact.semanticHash === headers.get(IDENTITY_HEADERS.semanticHash) &&
    artifact.etag === headers.get("etag");
}

async function errorCode(response: Response): Promise<string> {
  let body: unknown;
  try { body = await response.json(); } catch { failure(`HTTP ${response.status} had a malformed error body`); }
  if (typeof body !== "object" || body === null || Array.isArray(body) || typeof (body as { code?: unknown }).code !== "string") {
    failure(`HTTP ${response.status} had a malformed error body`);
  }
  return (body as { code: string }).code;
}

async function parseResponseJson(response: Response): Promise<{ payload: unknown; payloadText: string; transportBytes: ArrayBuffer }> {
  const bytes = await response.arrayBuffer();
  // The route deliberately sends an explicit gzip artifact rather than HTTP
  // Content-Encoding gzip: Fetch would otherwise decode the bytes before this
  // code can verify the compressed-representation ETag.
  if (response.headers.has("content-encoding")) failure("artifact transport must expose raw gzip bytes");
  if (typeof DecompressionStream === "undefined") failure("browser cannot decode gzip artifact");
  let text: string;
  try {
    const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream("gzip"));
    text = await new Response(stream).text();
  } catch { failure("artifact gzip could not be parsed"); }
  // The decompressed text is the producer's own serialization; the validator
  // recomputes the semantic hash over these exact bytes, never a re-stringify.
  try { return { payload: JSON.parse(text), payloadText: text, transportBytes: bytes }; } catch { failure("artifact JSON could not be parsed"); }
}

/**
 * Reads the immutable REST artifact. Only explicit rollout states may ask the
 * caller to use MCP; every transport, authorization, parse, and identity fault
 * stays a hard error.
 */
export async function fetchGalaxyArtifact(
  projectId: string,
  options: { signal?: AbortSignal } = {},
): Promise<GalaxyArtifactOutcome> {
  const cached = cache.get(projectId);
  const headers = new Headers({ Accept: "application/json" });
  if (cached?.size) headers.set("If-None-Match", [...cached.keys()].join(", "));
  let response: Response;
  try {
    response = await fetch(`${getServerBaseUrl()}/api/projects/${encodeURIComponent(projectId)}/code-graph/galaxy`, {
      credentials: "include", headers, signal: options.signal,
    });
  } catch (error) {
    if (error instanceof DOMException && error.name === "AbortError") throw error;
    failure(error instanceof Error ? error.message : "network failure");
  }
  if (response.status === 304) {
    const etag = response.headers.get("etag");
    const artifact = etag ? cached?.get(etag) : undefined;
    if (!artifact || !cacheMatchesRequest(projectId, artifact, response.headers)) failure("304 did not match a fully validated cache entry");
    return { kind: "artifact", artifact };
  }
  if (response.status === 404 || response.status === 409) {
    const code = await errorCode(response);
    if (response.status === 404 && code === UNAVAILABLE) return { kind: "fallback", reason: "unavailable" };
    if (response.status === 409 && code === UNSUPPORTED) return { kind: "fallback", reason: "unsupported" };
    failure(`unexpected rollout response ${response.status}/${code}`);
  }
  if (response.status === 401 || response.status === 403) failure(`authorization failed (${response.status})`);
  if (response.status !== 200) failure(`unexpected HTTP status ${response.status}`);
  const { payload, payloadText, transportBytes } = await parseResponseJson(response);
  const artifact = await validateGalaxyArtifact(projectId, response.headers, payload, payloadText, transportBytes);
  const projectCache = cache.get(projectId) ?? new Map<string, ValidatedGalaxyArtifact>();
  projectCache.set(artifact.etag, artifact);
  cache.set(projectId, projectCache);
  return { kind: "artifact", artifact };
}

/** Test/logout hook; production cache remains deliberately project-scoped in memory. */
export function clearGalaxyArtifactCache(): void { cache.clear(); }
