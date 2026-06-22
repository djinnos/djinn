import { describe, expect, it } from "vitest";

import {
  UNTAGGED_GROUP,
  isKnownParamLocation,
  parseOpenApiSpec,
  resolveRef,
  schemaExample,
} from "./openApiSpec";

/** Parse a spec object (stringified) and assert it succeeded, returning it. */
function parse(doc: unknown) {
  const result = parseOpenApiSpec(JSON.stringify(doc));
  if (!result.ok) {
    throw new Error(`expected ok parse, got error: ${result.error}`);
  }
  return result.spec;
}

describe("parseOpenApiSpec — top-level", () => {
  it("parses info, servers, and operation count", () => {
    const spec = parse({
      openapi: "3.0.0",
      info: { title: "Widgets API", version: "2.1.0", description: "Hello" },
      servers: [{ url: "https://api.example.com/v1" }],
      paths: {
        "/widgets": {
          get: { tags: ["widgets"], responses: { "200": { description: "OK" } } },
        },
      },
    });
    expect(spec.title).toBe("Widgets API");
    expect(spec.version).toBe("2.1.0");
    expect(spec.description).toBe("Hello");
    expect(spec.format).toBe("OpenAPI 3");
    expect(spec.servers).toEqual(["https://api.example.com/v1"]);
    expect(spec.operationCount).toBe(1);
  });

  it("detects Swagger 2 + host/basePath servers", () => {
    const spec = parse({
      swagger: "2.0",
      info: { title: "Legacy", version: "1.0" },
      host: "api.example.com",
      basePath: "/v2",
      paths: { "/ping": { get: { responses: { "200": {} } } } },
    });
    expect(spec.format).toBe("Swagger 2");
    expect(spec.servers).toEqual(["api.example.com/v2"]);
  });

  it("returns an error for empty input", () => {
    const result = parseOpenApiSpec("   ");
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.error).toMatch(/empty/i);
  });

  it("returns an error for invalid JSON, hinting YAML when it looks like YAML", () => {
    const result = parseOpenApiSpec("openapi: 3.0.0\ninfo:\n  title: X");
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.error).toMatch(/YAML is not supported/i);
  });

  it("rejects valid JSON that is not an OpenAPI document (graceful fallback)", () => {
    const result = parseOpenApiSpec('{"hello":"world","count":3}');
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.error).toMatch(/not an openapi/i);
  });

  it("does not throw on a non-object JSON document", () => {
    const result = parseOpenApiSpec("[1, 2, 3]");
    expect(result.ok).toBe(false);
  });
});

describe("parseOpenApiSpec — tag grouping", () => {
  it("groups operations by their declared tags", () => {
    const spec = parse({
      openapi: "3.0.0",
      info: { title: "T", version: "1" },
      tags: [
        { name: "pets", description: "Manage pets" },
        { name: "store" },
      ],
      paths: {
        "/pets": {
          get: { tags: ["pets"], responses: { "200": {} } },
          post: { tags: ["pets"], responses: { "201": {} } },
        },
        "/store/order": {
          get: { tags: ["store"], responses: { "200": {} } },
        },
      },
    });
    expect(spec.groups.map((g) => g.tag)).toEqual(["pets", "store"]);
    expect(spec.groups[0].operations).toHaveLength(2);
    expect(spec.groups[0].description).toBe("Manage pets");
    expect(spec.groups[1].operations).toHaveLength(1);
  });

  it("buckets operations with no tag into the untagged default group, sunk last", () => {
    const spec = parse({
      openapi: "3.0.0",
      info: { title: "T", version: "1" },
      tags: [{ name: "alpha" }],
      paths: {
        "/a": { get: { tags: ["alpha"], responses: { "200": {} } } },
        "/loose": { get: { responses: { "200": {} } } },
      },
    });
    const tags = spec.groups.map((g) => g.tag);
    expect(tags).toContain(UNTAGGED_GROUP);
    expect(tags[tags.length - 1]).toBe(UNTAGGED_GROUP);
    const untagged = spec.groups.find((g) => g.tag === UNTAGGED_GROUP);
    expect(untagged!.operations).toHaveLength(1);
    expect(untagged!.operations[0].path).toBe("/loose");
  });

  it("orders documented tags first, then undocumented tags alphabetically", () => {
    const spec = parse({
      openapi: "3.0.0",
      info: { title: "T", version: "1" },
      tags: [{ name: "zeta" }],
      paths: {
        "/z": { get: { tags: ["zeta"], responses: { "200": {} } } },
        "/b": { get: { tags: ["beta"], responses: { "200": {} } } },
        "/a": { get: { tags: ["alpha"], responses: { "200": {} } } },
      },
    });
    expect(spec.groups.map((g) => g.tag)).toEqual(["zeta", "alpha", "beta"]);
  });
});

describe("parseOpenApiSpec — params + responses", () => {
  it("normalizes path/query params and merges path-level params", () => {
    const spec = parse({
      openapi: "3.0.0",
      info: { title: "T", version: "1" },
      paths: {
        "/pets/{id}": {
          parameters: [
            {
              name: "id",
              in: "path",
              required: true,
              schema: { type: "string" },
            },
          ],
          get: {
            parameters: [
              { name: "verbose", in: "query", schema: { type: "boolean" } },
            ],
            responses: { "200": { description: "OK" }, "404": {} },
          },
        },
      },
    });
    const op = spec.groups[0].operations[0];
    expect(op.params.map((p) => p.name)).toEqual(["id", "verbose"]);
    expect(op.params[0].in).toBe("path");
    expect(op.params[0].required).toBe(true);
    expect(op.params[0].type).toBe("string");
    expect(op.params[1].type).toBe("boolean");
    expect(op.responses.map((r) => r.status)).toEqual(["200", "404"]);
  });

  it("marks operations secured by a global security requirement", () => {
    const spec = parse({
      openapi: "3.0.0",
      info: { title: "T", version: "1" },
      security: [{ apiKey: [] }],
      paths: { "/secret": { get: { responses: { "200": {} } } } },
    });
    expect(spec.groups[0].operations[0].secured).toBe(true);
  });

  it("lets an operation opt out of global security with security: []", () => {
    const spec = parse({
      openapi: "3.0.0",
      info: { title: "T", version: "1" },
      security: [{ apiKey: [] }],
      paths: { "/open": { get: { security: [], responses: { "200": {} } } } },
    });
    expect(spec.groups[0].operations[0].secured).toBe(false);
  });
});

describe("resolveRef — $ref resolution + cycle guard", () => {
  const root = {
    components: {
      schemas: {
        User: { type: "object", properties: { name: { type: "string" } } },
        "Weird/Name": { type: "string" },
      },
    },
  };

  it("resolves a nested local $ref", () => {
    const resolved = resolveRef(
      root,
      "#/components/schemas/User",
      new Set(),
    ) as Record<string, unknown>;
    expect(resolved.type).toBe("object");
  });

  it("decodes JSON-pointer escapes (~1 → /)", () => {
    const resolved = resolveRef(
      root,
      "#/components/schemas/Weird~1Name",
      new Set(),
    ) as Record<string, unknown>;
    expect(resolved.type).toBe("string");
  });

  it("returns undefined for external refs", () => {
    expect(resolveRef(root, "https://x/User", new Set())).toBeUndefined();
  });

  it("returns undefined (cycle guard) when the ref is already on the path", () => {
    const seen = new Set(["#/components/schemas/User"]);
    expect(
      resolveRef(root, "#/components/schemas/User", seen),
    ).toBeUndefined();
  });
});

describe("schemaExample — generation from schema", () => {
  const root = {};

  it("honors an explicit example", () => {
    expect(schemaExample(root, { type: "string", example: "abc" }, new Set(), 0)).toBe(
      "abc",
    );
  });

  it("honors default then enum then a type placeholder", () => {
    expect(
      schemaExample(root, { type: "string", default: "d" }, new Set(), 0),
    ).toBe("d");
    expect(
      schemaExample(root, { type: "string", enum: ["a", "b"] }, new Set(), 0),
    ).toBe("a");
    expect(schemaExample(root, { type: "string" }, new Set(), 0)).toBe(
      "string",
    );
  });

  it("emits typed placeholders for primitives", () => {
    expect(schemaExample(root, { type: "integer" }, new Set(), 0)).toBe(0);
    expect(schemaExample(root, { type: "boolean" }, new Set(), 0)).toBe(true);
    expect(
      schemaExample(root, { type: "string", format: "uuid" }, new Set(), 0),
    ).toBe("00000000-0000-0000-0000-000000000000");
  });

  it("builds an object example from properties", () => {
    const value = schemaExample(
      root,
      {
        type: "object",
        properties: {
          id: { type: "string" },
          count: { type: "integer" },
        },
      },
      new Set(),
      0,
    );
    expect(value).toEqual({ id: "string", count: 0 });
  });

  it("builds an array example with one representative item", () => {
    const value = schemaExample(
      root,
      { type: "array", items: { type: "string" } },
      new Set(),
      0,
    );
    expect(value).toEqual(["string"]);
  });

  it("resolves a nested $ref inside the example", () => {
    const refRoot = {
      components: {
        schemas: {
          Address: {
            type: "object",
            properties: { city: { type: "string" } },
          },
        },
      },
    };
    const value = schemaExample(
      refRoot,
      {
        type: "object",
        properties: { home: { $ref: "#/components/schemas/Address" } },
      },
      new Set(),
      0,
    );
    expect(value).toEqual({ home: { city: "string" } });
  });

  it("terminates on a circular $ref instead of recursing forever", () => {
    const refRoot = {
      components: {
        schemas: {
          Node: {
            type: "object",
            properties: {
              value: { type: "string" },
              next: { $ref: "#/components/schemas/Node" },
            },
          },
        },
      },
    };
    // Must not throw / hang. The cycle guard short-circuits the self-reference.
    const value = schemaExample(
      refRoot,
      { $ref: "#/components/schemas/Node" },
      new Set(),
      0,
    ) as Record<string, unknown>;
    expect(value.value).toBe("string");
    // The recursive `next` collapses to the unresolved ref node (no infinite tree).
    expect(JSON.stringify(value).length).toBeLessThan(500);
  });
});

describe("parseOpenApiSpec — request body + response examples from $ref", () => {
  it("derives request + response example bodies from referenced schemas", () => {
    const spec = parse({
      openapi: "3.0.0",
      info: { title: "T", version: "1" },
      paths: {
        "/pets": {
          post: {
            tags: ["pets"],
            requestBody: {
              content: {
                "application/json": {
                  schema: { $ref: "#/components/schemas/NewPet" },
                },
              },
            },
            responses: {
              "201": {
                description: "Created",
                content: {
                  "application/json": {
                    schema: { $ref: "#/components/schemas/Pet" },
                  },
                },
              },
            },
          },
        },
      },
      components: {
        schemas: {
          NewPet: {
            type: "object",
            properties: { name: { type: "string" } },
          },
          Pet: {
            type: "object",
            properties: {
              id: { type: "integer" },
              name: { type: "string" },
            },
          },
        },
      },
    });
    const op = spec.groups[0].operations[0];
    expect(op.requestContentType).toBe("application/json");
    expect(op.requestExample).toContain('"name": "string"');
    const created = op.responses.find((r) => r.status === "201");
    expect(created!.example).toContain('"id": 0');
    expect(created!.example).toContain('"name": "string"');
  });

  it("handles a circular model in a response without throwing", () => {
    const result = parseOpenApiSpec(
      JSON.stringify({
        openapi: "3.0.0",
        info: { title: "T", version: "1" },
        paths: {
          "/tree": {
            get: {
              responses: {
                "200": {
                  content: {
                    "application/json": {
                      schema: { $ref: "#/components/schemas/Node" },
                    },
                  },
                },
              },
            },
          },
        },
        components: {
          schemas: {
            Node: {
              type: "object",
              properties: {
                id: { type: "string" },
                children: {
                  type: "array",
                  items: { $ref: "#/components/schemas/Node" },
                },
              },
            },
          },
        },
      }),
    );
    expect(result.ok).toBe(true);
  });
});

describe("isKnownParamLocation", () => {
  it("recognizes the colored locations", () => {
    expect(isKnownParamLocation("path")).toBe(true);
    expect(isKnownParamLocation("query")).toBe(true);
    expect(isKnownParamLocation("header")).toBe(true);
    expect(isKnownParamLocation("cookie")).toBe(false);
  });
});
