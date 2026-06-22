import { describe, expect, it } from "vitest";

import {
  containerEntries,
  containerSummary,
  formatLeaf,
  isContainer,
  isNonEmptyContainer,
  parseJsonValue,
  valueKind,
  type JsonObject,
} from "./jsonExplorer";

describe("parseJsonValue", () => {
  it("parses a JSON object", () => {
    const result = parseJsonValue('{ "id": "abc", "active": true }');
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.value).toEqual({ id: "abc", active: true });
    }
  });

  it("parses a JSON array", () => {
    const result = parseJsonValue("[1, 2, 3]");
    expect(result.ok).toBe(true);
    if (result.ok) expect(result.value).toEqual([1, 2, 3]);
  });

  it("parses nested structures", () => {
    const result = parseJsonValue(
      '{ "user": { "roles": ["admin", "ops"] }, "count": 2 }',
    );
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.value).toEqual({
        user: { roles: ["admin", "ops"] },
        count: 2,
      });
    }
  });

  it("parses primitives at the root", () => {
    expect(parseJsonValue("42")).toEqual({ ok: true, value: 42 });
    expect(parseJsonValue('"hi"')).toEqual({ ok: true, value: "hi" });
    expect(parseJsonValue("true")).toEqual({ ok: true, value: true });
    expect(parseJsonValue("null")).toEqual({ ok: true, value: null });
  });

  it("tolerates leading/trailing whitespace", () => {
    const result = parseJsonValue('\n  { "ok": true }\n');
    expect(result.ok).toBe(true);
  });

  it("fails gracefully on invalid JSON with an error message", () => {
    const result = parseJsonValue("{ not: valid json }");
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.error.length).toBeGreaterThan(0);
  });

  it("treats empty / whitespace input as a recoverable failure", () => {
    expect(parseJsonValue("").ok).toBe(false);
    expect(parseJsonValue("   \n ").ok).toBe(false);
  });
});

describe("valueKind", () => {
  it("classifies every JSON value kind", () => {
    expect(valueKind("s")).toBe("string");
    expect(valueKind(3)).toBe("number");
    expect(valueKind(true)).toBe("boolean");
    expect(valueKind(null)).toBe("null");
    expect(valueKind([1])).toBe("array");
    expect(valueKind({ a: 1 })).toBe("object");
  });
});

describe("container helpers", () => {
  it("isContainer distinguishes containers from primitives and null", () => {
    expect(isContainer({ a: 1 })).toBe(true);
    expect(isContainer([1])).toBe(true);
    expect(isContainer(null)).toBe(false);
    expect(isContainer("x")).toBe(false);
    expect(isContainer(1)).toBe(false);
  });

  it("isNonEmptyContainer is false for empty containers", () => {
    expect(isNonEmptyContainer({})).toBe(false);
    expect(isNonEmptyContainer([])).toBe(false);
    expect(isNonEmptyContainer({ a: 1 })).toBe(true);
    expect(isNonEmptyContainer([1])).toBe(true);
    expect(isNonEmptyContainer("x")).toBe(false);
  });

  it("containerEntries yields indexed entries for arrays and keyed for objects", () => {
    expect(containerEntries(["a", "b"])).toEqual([
      [0, "a"],
      [1, "b"],
    ]);
    const obj: JsonObject = { id: 1, name: "x" };
    expect(containerEntries(obj)).toEqual([
      ["id", 1],
      ["name", "x"],
    ]);
  });

  it("containerSummary is singular/plural aware", () => {
    expect(containerSummary([1])).toBe("1 item");
    expect(containerSummary([1, 2])).toBe("2 items");
    expect(containerSummary({ a: 1 })).toBe("1 key");
    expect(containerSummary({ a: 1, b: 2 })).toBe("2 keys");
  });
});

describe("formatLeaf", () => {
  it("quotes strings, lowercases null, stringifies number/boolean", () => {
    expect(formatLeaf("hi")).toBe('"hi"');
    expect(formatLeaf(null)).toBe("null");
    expect(formatLeaf(42)).toBe("42");
    expect(formatLeaf(true)).toBe("true");
  });
});
