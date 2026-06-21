import { describe, expect, it } from "vitest";

import {
  classifyStatus,
  hasStructuredContent,
  mentionsAuth,
  mentionsDeprecated,
  normalizeMethod,
  parseApiEndpointBody,
} from "./apiEndpoint";

describe("normalizeMethod", () => {
  it("uppercases + accepts known verbs", () => {
    expect(normalizeMethod("get")).toBe("GET");
    expect(normalizeMethod("  Post ")).toBe("POST");
    expect(normalizeMethod("DELETE")).toBe("DELETE");
    expect(normalizeMethod("patch")).toBe("PATCH");
    expect(normalizeMethod("options")).toBe("OPTIONS");
  });

  it("returns undefined for unknown / empty methods", () => {
    expect(normalizeMethod("TRACE")).toBeUndefined();
    expect(normalizeMethod("")).toBeUndefined();
    expect(normalizeMethod(undefined)).toBeUndefined();
  });
});

describe("classifyStatus", () => {
  it("maps by leading digit", () => {
    expect(classifyStatus("200")).toBe("success");
    expect(classifyStatus("201")).toBe("success");
    expect(classifyStatus("2xx")).toBe("success");
    expect(classifyStatus("400")).toBe("warn");
    expect(classifyStatus("404")).toBe("warn");
    expect(classifyStatus("4XX")).toBe("warn");
    expect(classifyStatus("500")).toBe("error");
    expect(classifyStatus("503")).toBe("error");
  });

  it("treats 1xx / 3xx / default / other as info", () => {
    expect(classifyStatus("100")).toBe("info");
    expect(classifyStatus("301")).toBe("info");
    expect(classifyStatus("default")).toBe("info");
    expect(classifyStatus("")).toBe("info");
  });
});

describe("mentionsAuth / mentionsDeprecated", () => {
  it("detects auth keywords", () => {
    expect(mentionsAuth("Requires a Bearer token.")).toBe(true);
    expect(mentionsAuth("Authentication via OAuth.")).toBe(true);
    expect(mentionsAuth("Pass an API key header.")).toBe(true);
    expect(mentionsAuth("Returns a list of users.")).toBe(false);
  });

  it("detects deprecated marker", () => {
    expect(mentionsDeprecated("This endpoint is deprecated.")).toBe(true);
    expect(mentionsDeprecated("Stable endpoint.")).toBe(false);
  });
});

describe("parseApiEndpointBody — fallback / prose only", () => {
  it("returns no structured content for empty input", () => {
    const parsed = parseApiEndpointBody("");
    expect(parsed.params).toEqual([]);
    expect(parsed.responses).toEqual([]);
    expect(hasStructuredContent(parsed)).toBe(false);
  });

  it("keeps a plain description with no structured sections", () => {
    const parsed = parseApiEndpointBody("Returns a list of all users.");
    expect(parsed.description).toBe("Returns a list of all users.");
    expect(parsed.params).toEqual([]);
    expect(parsed.responses).toEqual([]);
    expect(hasStructuredContent(parsed)).toBe(false);
  });
});

describe("parseApiEndpointBody — parameters table", () => {
  it("parses a name/in/type/required/description table", () => {
    const body = [
      "Fetch a user.",
      "",
      "## Parameters",
      "",
      "| name  | in    | type | required | description    |",
      "| ----- | ----- | ---- | -------- | -------------- |",
      "| id    | path  | uuid | yes      | the user id    |",
      "| limit | query | int  | no       | page size      |",
      "| token | header| str  | required | bearer token   |",
    ].join("\n");
    const parsed = parseApiEndpointBody(body);

    expect(parsed.description).toBe("Fetch a user.");
    expect(parsed.params).toHaveLength(3);

    expect(parsed.params[0]).toMatchObject({
      name: "id",
      in: "path",
      type: "uuid",
      required: true,
      description: "the user id",
    });
    expect(parsed.params[1]).toMatchObject({
      name: "limit",
      in: "query",
      type: "int",
      required: false,
    });
    expect(parsed.params[2]).toMatchObject({ name: "token", in: "header" });
    expect(hasStructuredContent(parsed)).toBe(true);
  });

  it("parses a bulleted parameter list with qualifiers", () => {
    const body = [
      "### Params",
      "- `id` (path, uuid, required) — the user id",
      "- `q` (query, string) — search term",
    ].join("\n");
    const parsed = parseApiEndpointBody(body);

    expect(parsed.params).toHaveLength(2);
    expect(parsed.params[0]).toMatchObject({
      name: "id",
      in: "path",
      type: "uuid",
      required: true,
      description: "the user id",
    });
    expect(parsed.params[1]).toMatchObject({
      name: "q",
      in: "query",
      type: "string",
      description: "search term",
    });
  });
});

describe("parseApiEndpointBody — responses", () => {
  it("parses a status/description responses table", () => {
    const body = [
      "## Responses",
      "",
      "| status | description     |",
      "| ------ | --------------- |",
      "| 200    | the user object |",
      "| 404    | not found       |",
      "| 500    | server error    |",
    ].join("\n");
    const parsed = parseApiEndpointBody(body);

    expect(parsed.responses).toHaveLength(3);
    expect(parsed.responses[0]).toMatchObject({
      status: "200",
      description: "the user object",
    });
    expect(parsed.responses[1]).toMatchObject({ status: "404" });
    expect(classifyStatus(parsed.responses[2]!.status)).toBe("error");
  });

  it("parses a bulleted responses list", () => {
    const body = [
      "## Responses",
      "- 200 — OK, returns the resource",
      "- 401: unauthorized",
    ].join("\n");
    const parsed = parseApiEndpointBody(body);

    expect(parsed.responses).toHaveLength(2);
    expect(parsed.responses[0]).toMatchObject({
      status: "200",
      description: "OK, returns the resource",
    });
    expect(parsed.responses[1]).toMatchObject({
      status: "401",
      description: "unauthorized",
    });
  });

  it("attaches a fenced JSON example to a response", () => {
    const body = [
      "## Responses",
      "- 200 — the user",
      "```json",
      '{ "id": "abc", "name": "Ada" }',
      "```",
    ].join("\n");
    const parsed = parseApiEndpointBody(body);

    expect(parsed.responses).toHaveLength(1);
    expect(parsed.responses[0]!.example).toContain('"name": "Ada"');
    expect(hasStructuredContent(parsed)).toBe(true);
  });
});

describe("parseApiEndpointBody — request body", () => {
  it("captures a request-section fenced body as the request example", () => {
    const body = [
      "Create a user.",
      "",
      "## Request Body",
      "```json",
      '{ "name": "Ada" }',
      "```",
    ].join("\n");
    const parsed = parseApiEndpointBody(body);

    expect(parsed.description).toBe("Create a user.");
    expect(parsed.requestExample).toContain('"name": "Ada"');
    expect(hasStructuredContent(parsed)).toBe(true);
  });
});

describe("parseApiEndpointBody — robustness", () => {
  it("does not split a section on a heading-like line inside a code fence", () => {
    const body = [
      "Describe it.",
      "```",
      "## Parameters (this is inside a fence, not a heading)",
      "```",
    ].join("\n");
    const parsed = parseApiEndpointBody(body);
    // No real Parameters section was opened, so no params parsed.
    expect(parsed.params).toEqual([]);
  });
});
