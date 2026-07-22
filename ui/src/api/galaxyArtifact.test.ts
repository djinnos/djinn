import { createHash, webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { gzipSync } from "node:zlib";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@/api/serverUrl", () => ({ getServerBaseUrl: () => "http://server.test" }));

import { clearGalaxyArtifactCache, fetchGalaxyArtifact } from "./galaxyArtifact";
import {
  IDENTITY_HEADERS,
  canonicalSemanticJson,
  validateGalaxyArtifact,
} from "./galaxyArtifactSchema";

// Resolve the producer's golden fixtures from a plain filesystem path. Building
// it from `fileURLToPath(import.meta.url)` avoids Vite's `new URL(<literal>,
// import.meta.url)` asset rewrite, which would otherwise turn the fixture path
// into a non-file `/@fs/` URL that `readFile` rejects.
const fixtureDir = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../server/crates/djinn-graph/src/galaxy_artifact/fixtures",
);
const fixturePath = (name: string) => path.join(fixtureDir, name);
let payloadText: string;
let hashInputText: string;
let payload: Record<string, unknown>;
let gzip: Uint8Array;
let manifest: { graph_content_hash: string; transport_sha256: string };

beforeAll(async () => {
  // The browser client uses Web Crypto; jsdom supplies no subtle implementation.
  if (!globalThis.crypto?.subtle) Object.defineProperty(globalThis, "crypto", { value: webcrypto });
  [payloadText, hashInputText, gzip, manifest] = await Promise.all([
    readFile(fixturePath("payload.json"), "utf8"),
    // The producer's exact hash-input bytes: the final payload with only the
    // graph_content_hash field absent. The client rehashes over these bytes.
    readFile(fixturePath("hash_input.json"), "utf8"),
    readFile(fixturePath("payload.json.gz")),
    readFile(fixturePath("manifest.json"), "utf8").then(JSON.parse),
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

/**
 * Splice the graph_content_hash field into producer-serialized bytes at its
 * fixed serde position (immediately after generation_id, immediately before
 * truncated), mirroring how the Rust producer serializes the final payload so
 * that final-minus-field is byte-identical to the hashed input.
 */
function spliceHashField(hashInput: string, graphContentHash: string): string {
  const marker = ',"truncated":';
  const at = hashInput.indexOf(marker);
  if (at === -1) throw new Error("test fixture is missing the truncated field");
  return `${hashInput.slice(0, at)},"graph_content_hash":"${graphContentHash}"${hashInput.slice(at)}`;
}

/** Derive valid later generations from the producer golden, never a hand schema. */
function generationResponse(generationId: string, generatedAt: string) {
  // Mirror the producer: hash the payload bytes with graph_content_hash absent,
  // then serialize the final payload with the hash spliced back into place.
  const next = structuredClone(payload);
  next.generation_id = generationId;
  next.generated_at = generatedAt;
  delete next.graph_content_hash;
  const hashInput = JSON.stringify(next);
  const graphContentHash = hash(hashInput);
  const bytes = gzipSync(spliceHashField(hashInput, graphContentHash));
  const etag = `"${hash(bytes)}"`;
  const responseHeaders = headers(etag, {
    [IDENTITY_HEADERS.generationId]: generationId,
    [IDENTITY_HEADERS.semanticHash]: graphContentHash,
    "content-type": "application/gzip",
  });
  return { etag, response: new Response(bytes, { status: 200, headers: responseHeaders }) };
}

describe("producer galaxy artifact golden", () => {
  it("independently hashes both producer domains and validates the real payload", async () => {
    // This is intentionally Node crypto rather than the validator helper.
    expect(hash(gzip)).toBe(manifest.transport_sha256);
    // The semantic domain is the payload bytes with only the hash field spliced
    // out — byte-identical to the producer's separate hash_input.json.
    const spliced = canonicalSemanticJson(payloadText, manifest.graph_content_hash);
    expect(spliced).toBe(hashInputText);
    expect(hash(spliced)).toBe(manifest.graph_content_hash);

    const validated = await validateGalaxyArtifact(
      "test-project",
      headers(`"${manifest.transport_sha256}"`),
      payload,
      payloadText,
      gzip,
    );
    expect(validated.snapshot.nodes).toHaveLength(5);
  });

  it("rejects valid JSON when the syntactically strong ETag is not its transport hash", async () => {
    await expect(validateGalaxyArtifact(
      "test-project", headers(`"${"a".repeat(64)}"`), payload, payloadText, gzip,
    )).rejects.toThrow("transport etag recomputation mismatch");
  });

  it("refuses to guess when the semantic hash field is not uniquely spliceable", () => {
    const h = "b".repeat(64);
    const literal = `"graph_content_hash":"${h}",`;
    expect(() => canonicalSemanticJson(`{${literal}"truncated":false}${literal}`, h))
      .toThrow("semantic hash field is not uniquely spliceable");
    expect(() => canonicalSemanticJson('{"truncated":false}', h))
      .toThrow("semantic hash field is not uniquely spliceable");
  });
});

describe("producer float formatting is not a JS re-serialization fixed point", () => {
  it("validates producer bytes whose integral floats JSON.stringify would renormalize", async () => {
    // serde_json/ryu emits an integral f64 as 0.0 / 1.0; JSON.parse->JSON.stringify
    // collapses these to 0 / 1, so a re-stringify would never rehash. The client
    // must hash the producer's raw bytes with only the hash field spliced out.
    const hashInput =
      '{"project_id":"test-project","git_head":"abc123","generated_at":"2026-07-18T00:00:00Z"' +
      ',"generation_id":"019f741c-0000-7000-8000-000000000000","truncated":false' +
      ',"total_nodes":1,"total_edges":1,"node_cap":1' +
      ',"nodes":[{"id":"file:src/app.rs","uid":"file:src/app.rs","kind":"file"' +
      ',"label":"src/app.rs","pagerank":0.0,"is_test":false,"x":1.0,"y":0.0}]' +
      ',"edges":[{"from":"file:src/app.rs","to":"file:src/app.rs"' +
      ',"kind":"ContainsDefinition","confidence":1.0}]}';
    // Proof this exercises the bug: a JS round-trip is not byte-stable here.
    expect(JSON.stringify(JSON.parse(hashInput))).not.toBe(hashInput);

    const graphContentHash = hash(hashInput);
    const bytes = gzipSync(spliceHashField(hashInput, graphContentHash));
    const etag = `"${hash(bytes)}"`;
    const fetchMock = vi.fn().mockResolvedValueOnce(new Response(bytes, {
      status: 200,
      headers: headers(etag, {
        [IDENTITY_HEADERS.semanticHash]: graphContentHash,
        "content-type": "application/gzip",
      }),
    }));
    vi.stubGlobal("fetch", fetchMock);

    const outcome = await fetchGalaxyArtifact("test-project");
    expect(outcome).toMatchObject({ kind: "artifact" });
    if (outcome.kind === "artifact") {
      expect(outcome.artifact.snapshot.nodes[0].pagerank).toBe(0);
      expect(outcome.artifact.snapshot.nodes[0].x).toBe(1);
    }
  });

  it("rejects tampered payload bytes even when transport and identity self-agree", async () => {
    // Flip one semantic byte (a pagerank digit) while leaving the graph_content_hash
    // field intact, and recompute the transport etag over the tampered gzip so the
    // transport and identity checks pass. The spliced rehash must still catch it.
    const tampered = payloadText.replace('"pagerank":0.38662203610895207', '"pagerank":0.38662203610895208');
    expect(tampered).not.toBe(payloadText);
    const bytes = gzipSync(tampered);
    const etag = `"${hash(bytes)}"`;
    const fetchMock = vi.fn().mockResolvedValueOnce(new Response(bytes, {
      status: 200,
      headers: headers(etag, { "content-type": "application/gzip" }),
    }));
    vi.stubGlobal("fetch", fetchMock);
    await expect(fetchGalaxyArtifact("test-project")).rejects.toThrow("semantic hash recomputation mismatch");
  });
});

describe("synthetic node kinds and empty content fields the producer can emit", () => {
  it("accepts process/table/route kinds and empty content strings, normalizing kinds to symbol", async () => {
    // The producer emits `kind` as `format!("{:?}", node.kind).to_lowercase()`
    // over `RepoGraphNodeKind`, whose variants include synthetic Process / Table
    // / Route / Tool nodes. These sort to the tail (pagerank 0.0). The old
    // validator hard-failed on any kind outside file/folder/symbol/community —
    // stricter than both the producer and the adapter (which folds unknown kinds
    // to "symbol"). Empty content strings are also legal from the producer: a
    // Table node whose `display_name` prettifies to "" yields an empty `label`,
    // and optional `Option<String>` fields serialize as "" when the source value
    // is empty. The validator must tolerate all of these while still rehashing.
    const hashInput =
      '{"project_id":"test-project","git_head":"abc123","generated_at":"2026-07-18T00:00:00Z"' +
      ',"generation_id":"019f741c-0000-7000-8000-000000000000","truncated":false' +
      ',"total_nodes":4,"total_edges":1,"node_cap":4' +
      ',"nodes":[' +
      '{"id":"file:src/app.rs","uid":"file:src/app.rs","kind":"file","label":"src/app.rs","pagerank":1.0,"is_test":false,"x":0.0,"y":0.0}' +
      ',{"id":"process:abc123","uid":"process:abc123","kind":"process","label":"main process","symbol_kind":"","file_path":"","pagerank":0.0,"community_id":"","workspace":"","is_test":false,"x":0.0,"y":0.0}' +
      ',{"id":"table:public.users","uid":"table:public.users","kind":"table","label":"","pagerank":0.0,"is_test":false,"x":0.0,"y":0.0}' +
      ',{"id":"route:GET /api (axum)","uid":"route:GET /api (axum)","kind":"route","label":"GET /api (axum)","pagerank":0.0,"is_test":false,"x":0.0,"y":0.0}' +
      ']' +
      // One structural edge + one co-change sidecar edge appended after it.
      // total_edges counts STRUCTURAL edges only (1), so edges.length (2) exceeds
      // it by exactly the co-change count — the producer's convention.
      ',"edges":[{"from":"file:src/app.rs","to":"process:abc123","kind":"StepInProcess","confidence":1.0,"reason":""}' +
      ',{"from":"file:src/app.rs","to":"table:public.users","kind":"CoChangedWith","confidence":0.5,"reason":"cochange;last_day=0"}]}';
    const graphContentHash = hash(hashInput);
    const payloadText = spliceHashField(hashInput, graphContentHash);
    const bytes = gzipSync(payloadText);
    const etag = `"${hash(bytes)}"`;

    const validated = await validateGalaxyArtifact(
      "test-project",
      headers(etag, { [IDENTITY_HEADERS.semanticHash]: graphContentHash, "content-type": "application/gzip" }),
      JSON.parse(payloadText) as Record<string, unknown>,
      payloadText,
      bytes,
    );

    // Synthetic kinds normalize to "symbol" exactly like the renderer's adapter.
    expect(validated.snapshot.nodes.map((n) => n.kind)).toEqual(["file", "symbol", "symbol", "symbol"]);
    // Empty content strings survive validation with their type preserved.
    const process = validated.snapshot.nodes[1];
    expect(process.label).toBe("main process");
    expect(process.symbol_kind).toBe("");
    expect(process.file_path).toBe("");
    expect(process.community_id).toBe("");
    expect(process.workspace).toBe("");
    expect(validated.snapshot.nodes[2].label).toBe("");
    expect(validated.snapshot.edges[0].reason).toBe("");
    // The co-change sidecar rides beyond total_edges: edges.length (2) exceeds
    // total_edges (1, structural only). The old raw-length equality would have
    // thrown "payload counts do not match contents"; the structural-only count
    // is what actually matches the producer.
    expect(validated.snapshot.edges).toHaveLength(2);
    expect(validated.snapshot.total_edges).toBe(1);
    expect(validated.snapshot.edges[1].kind).toBe("CoChangedWith");
    expect(validated.snapshot.edges.length).not.toBe(validated.snapshot.total_edges);
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
