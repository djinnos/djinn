import { createHash, webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import { gzipSync } from "node:zlib";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@/api/serverUrl", () => ({ getServerBaseUrl: () => "http://server.test" }));

import { clearGalaxyArtifactCache, fetchGalaxyArtifact } from "./galaxyArtifact";
import {
  IDENTITY_HEADERS,
  canonicalSemanticJson,
  validateGalaxyArtifact,
} from "./galaxyArtifactSchema";

const fixture = new URL("../../../server/crates/djinn-graph/src/galaxy_artifact/fixtures/", import.meta.url);
let payloadText: string;
let payload: Record<string, unknown>;
let gzip: Uint8Array;
let manifest: { graph_content_hash: string; transport_sha256: string };

beforeAll(async () => {
  // The browser client uses Web Crypto; jsdom supplies no subtle implementation.
  if (!globalThis.crypto?.subtle) Object.defineProperty(globalThis, "crypto", { value: webcrypto });
  [payloadText, gzip, manifest] = await Promise.all([
    readFile(new URL("payload.json", fixture), "utf8"),
    readFile(new URL("payload.json.gz", fixture)),
    readFile(new URL("manifest.json", fixture), "utf8").then(JSON.parse),
  ]);
  payload = JSON.parse(payloadText) as Record<string, unknown>;
});

beforeEach(() => {
  clearGalaxyArtifactCache();
  vi.restoreAllMocks();
});

function hash(bytes: Uint8Array | string): string {
  return createHash("sha256").update(bytes).digest("hex");
}

function headers(etag: string, overrides: Record<string, string> = {}): Headers {
  return new Headers({
    [IDENTITY_HEADERS.projectId]: "test-project",
    [IDENTITY_HEADERS.generationId]: "019f741c-0000-7000-8000-000000000000",
    [IDENTITY_HEADERS.commitSha]: "abc123",
    [IDENTITY_HEADERS.artifactVersion]: "1",
    [IDENTITY_HEADERS.semanticHash]: manifest.graph_content_hash,
    etag,
    ...overrides,
  });
}

function artifactResponse(etag = `"${manifest.transport_sha256}"`): Response {
  return new Response(gzip, { status: 200, headers: headers(etag, { "content-type": "application/gzip" }) });
}

/** Derive valid later generations from the producer golden, never a hand schema. */
function generationResponse(generationId: string, generatedAt: string) {
  const next = structuredClone(payload);
  next.generation_id = generationId;
  next.generated_at = generatedAt;
  next.graph_content_hash = hash(canonicalSemanticJson(next));
  const bytes = gzipSync(JSON.stringify(next));
  const etag = `"${hash(bytes)}"`;
  const responseHeaders = headers(etag, {
    [IDENTITY_HEADERS.generationId]: generationId,
    [IDENTITY_HEADERS.semanticHash]: next.graph_content_hash as string,
    "content-type": "application/gzip",
  });
  return { etag, response: new Response(bytes, { status: 200, headers: responseHeaders }) };
}

describe("producer galaxy artifact golden", () => {
  it("independently hashes both producer domains and validates the real payload", async () => {
    // This is intentionally Node crypto rather than the validator helper.
    expect(hash(gzip)).toBe(manifest.transport_sha256);
    expect(hash(canonicalSemanticJson(payload))).toBe(manifest.graph_content_hash);

    const validated = await validateGalaxyArtifact(
      "test-project",
      headers(`"${manifest.transport_sha256}"`),
      payload,
      gzip,
    );
    expect(validated.snapshot.nodes).toHaveLength(5);
  });

  it("rejects valid JSON when the syntactically strong ETag is not its transport hash", async () => {
    await expect(validateGalaxyArtifact(
      "test-project", headers(`"${"a".repeat(64)}"`), payload, gzip,
    )).rejects.toThrow("transport etag recomputation mismatch");
  });
});

describe("fetchGalaxyArtifact cache and rollout outcomes", () => {
  it("caches a validated 200 and only reuses it for a matching 304", async () => {
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(artifactResponse())
      .mockResolvedValueOnce(new Response(null, { status: 304, headers: headers(`"${manifest.transport_sha256}"`) }));
    vi.stubGlobal("fetch", fetchMock);

    const first = await fetchGalaxyArtifact("test-project");
    const second = await fetchGalaxyArtifact("test-project");
    expect(first.kind).toBe("artifact");
    expect(second).toEqual(first);
    expect((fetchMock.mock.calls[1][1] as RequestInit).headers).toEqual(expect.any(Headers));
    expect(((fetchMock.mock.calls[1][1] as RequestInit).headers as Headers).get("if-none-match")).toBe(`"${manifest.transport_sha256}"`);
  });

  it("rejects stale 304 identity and never treats it as a cache hit", async () => {
    vi.stubGlobal("fetch", vi.fn()
      .mockResolvedValueOnce(artifactResponse())
      .mockResolvedValueOnce(new Response(null, {
        status: 304,
        headers: headers(`"${manifest.transport_sha256}"`, { [IDENTITY_HEADERS.generationId]: "new-generation" }),
      })));
    await fetchGalaxyArtifact("test-project");
    await expect(fetchGalaxyArtifact("test-project")).rejects.toThrow("304 did not match");
  });

  it("keeps a valid artifact across artifactless advancement and reuses it on 304", async () => {
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(artifactResponse())
      .mockResolvedValueOnce(new Response(JSON.stringify({ code: "galaxy_artifact_unavailable" }), { status: 404 }))
      .mockResolvedValueOnce(new Response(null, { status: 304, headers: headers(`"${manifest.transport_sha256}"`) }));
    vi.stubGlobal("fetch", fetchMock);
    await fetchGalaxyArtifact("test-project");
    await expect(fetchGalaxyArtifact("test-project")).resolves.toEqual({ kind: "fallback", reason: "unavailable" });
    await expect(fetchGalaxyArtifact("test-project")).resolves.toMatchObject({ kind: "artifact" });
  });

  it("retains old ETags so a G1 to G2 to G1 rollback is a validated 304 reuse", async () => {
    const g2 = generationResponse("019f741c-0000-7000-8000-000000000002", "later");
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(artifactResponse())
      .mockResolvedValueOnce(g2.response)
      .mockResolvedValueOnce(new Response(null, { status: 304, headers: headers(`"${manifest.transport_sha256}"`) }));
    vi.stubGlobal("fetch", fetchMock);
    const g1 = await fetchGalaxyArtifact("test-project");
    await fetchGalaxyArtifact("test-project");
    const rollback = await fetchGalaxyArtifact("test-project");
    expect(rollback).toEqual(g1);
    expect(((fetchMock.mock.calls[2][1] as RequestInit).headers as Headers).get("if-none-match"))
      .toBe(`"${manifest.transport_sha256}", ${g2.etag}`);
  });

  it("allows MCP fallback only for the two explicit rollout codes", async () => {
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(new Response(JSON.stringify({ code: "galaxy_artifact_unavailable" }), { status: 404 }))
      .mockResolvedValueOnce(new Response(JSON.stringify({ code: "galaxy_artifact_unsupported" }), { status: 409 }))
      .mockResolvedValueOnce(new Response(JSON.stringify({ code: "other" }), { status: 404 }));
    vi.stubGlobal("fetch", fetchMock);
    await expect(fetchGalaxyArtifact("test-project")).resolves.toEqual({ kind: "fallback", reason: "unavailable" });
    await expect(fetchGalaxyArtifact("test-project")).resolves.toEqual({ kind: "fallback", reason: "unsupported" });
    await expect(fetchGalaxyArtifact("test-project")).rejects.toThrow("unexpected rollout response");
  });

  it("rejects transport corruption, mixed versions, and authorization instead of falling back", async () => {
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(artifactResponse(`"${"a".repeat(64)}"`))
      .mockResolvedValueOnce(new Response(gzip, { status: 200, headers: headers(`"${manifest.transport_sha256}"`, { [IDENTITY_HEADERS.artifactVersion]: "2" }) }))
      .mockResolvedValueOnce(new Response(null, { status: 401 }));
    vi.stubGlobal("fetch", fetchMock);
    await expect(fetchGalaxyArtifact("test-project")).rejects.toThrow("transport etag recomputation mismatch");
    await expect(fetchGalaxyArtifact("test-project")).rejects.toThrow("artifact version is unsupported");
    await expect(fetchGalaxyArtifact("test-project")).rejects.toThrow("authorization failed");
  });
});
