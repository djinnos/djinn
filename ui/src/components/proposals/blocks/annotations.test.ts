import { describe, expect, it } from "vitest";

import {
  buildLineMarkerMap,
  coerceAnnotation,
  hasResolvedAnnotations,
  parseAnnotationsAttr,
  parseLineRange,
  rangeLabel,
  resolveAnnotations,
} from "./annotations";

describe("parseLineRange", () => {
  it("parses a single 1-based line", () => {
    expect(parseLineRange("3", 10)).toEqual({ start: 3, end: 3 });
  });

  it("parses an inclusive range", () => {
    expect(parseLineRange("3-5", 10)).toEqual({ start: 3, end: 5 });
  });

  it("tolerates surrounding and inner whitespace", () => {
    expect(parseLineRange("  3 - 5 ", 10)).toEqual({ start: 3, end: 5 });
  });

  it("normalizes a reversed range", () => {
    expect(parseLineRange("5-3", 10)).toEqual({ start: 3, end: 5 });
  });

  it("clamps a partially out-of-range range to the file", () => {
    expect(parseLineRange("8-20", 10)).toEqual({ start: 8, end: 10 });
    expect(parseLineRange("0-2", 10)).toEqual({ start: 1, end: 2 });
  });

  it("returns null for a fully out-of-range ref", () => {
    expect(parseLineRange("20-25", 10)).toBeNull();
    expect(parseLineRange("5", 0)).toBeNull();
  });

  it("returns null for malformed refs", () => {
    expect(parseLineRange("abc", 10)).toBeNull();
    expect(parseLineRange("3-", 10)).toBeNull();
    expect(parseLineRange("", 10)).toBeNull();
    expect(parseLineRange("3-5-7", 10)).toBeNull();
  });
});

describe("coerceAnnotation", () => {
  it("accepts the new { lines } shape", () => {
    expect(coerceAnnotation({ lines: "3-5", note: "hi", label: "x" })).toEqual({
      lines: "3-5",
      note: "hi",
      label: "x",
    });
  });

  it("normalizes the legacy { line: number } shape", () => {
    expect(coerceAnnotation({ line: 4, note: "hi" })).toEqual({
      lines: "4",
      note: "hi",
    });
  });

  it("coerces a numeric `lines` value to a string", () => {
    expect(coerceAnnotation({ lines: 7, note: "n" })).toEqual({
      lines: "7",
      note: "n",
    });
  });

  it("drops empty labels", () => {
    expect(coerceAnnotation({ lines: "1", note: "n", label: "  " })).toEqual({
      lines: "1",
      note: "n",
    });
  });

  it("returns null when there is no usable line ref or content", () => {
    expect(coerceAnnotation({ note: "no line" })).toBeNull();
    expect(coerceAnnotation({ lines: "1" })).toBeNull();
    expect(coerceAnnotation(null)).toBeNull();
    expect(coerceAnnotation("nope")).toBeNull();
  });
});

describe("parseAnnotationsAttr", () => {
  it("returns an empty list for an absent/blank attr", () => {
    expect(parseAnnotationsAttr(undefined)).toEqual([]);
    expect(parseAnnotationsAttr("")).toEqual([]);
    expect(parseAnnotationsAttr("   ")).toEqual([]);
  });

  it("parses a valid annotation array (both shapes)", () => {
    const out = parseAnnotationsAttr(
      JSON.stringify([
        { line: 2, note: "legacy" },
        { lines: "4-6", note: "range", label: "L" },
      ]),
    );
    expect(out).toEqual([
      { lines: "2", note: "legacy" },
      { lines: "4-6", note: "range", label: "L" },
    ]);
  });

  it("returns null on invalid JSON (so callers can warn, not swallow)", () => {
    expect(parseAnnotationsAttr("{not json")).toBeNull();
  });

  it("returns null when the JSON is not an array", () => {
    expect(parseAnnotationsAttr('{"lines":"1","note":"x"}')).toBeNull();
  });

  it("drops malformed entries but keeps valid ones", () => {
    const out = parseAnnotationsAttr(
      JSON.stringify([{ note: "no line" }, { lines: "1", note: "ok" }]),
    );
    expect(out).toEqual([{ lines: "1", note: "ok" }]);
  });
});

describe("resolveAnnotations + line marker map", () => {
  it("assigns stable 1-based markers to all annotations", () => {
    const resolved = resolveAnnotations(
      [
        { lines: "2", note: "a" },
        { lines: "999", note: "out" },
        { lines: "4-5", note: "b" },
      ],
      10,
    );
    expect(resolved.map((r) => r.marker)).toEqual([1, 2, 3]);
    expect(resolved[1].range).toBeNull();
    expect(resolved[2].range).toEqual({ start: 4, end: 5 });
  });

  it("maps each covered line to its annotations", () => {
    const resolved = resolveAnnotations(
      [{ lines: "2-3", note: "a" }, { lines: "3", note: "b" }],
      10,
    );
    const map = buildLineMarkerMap(resolved);
    expect(map.get(2)?.map((r) => r.marker)).toEqual([1]);
    expect(map.get(3)?.map((r) => r.marker)).toEqual([1, 2]);
    expect(map.get(1)).toBeUndefined();
  });

  it("reports whether any annotation resolved", () => {
    expect(
      hasResolvedAnnotations(resolveAnnotations([{ lines: "1", note: "a" }], 5)),
    ).toBe(true);
    expect(
      hasResolvedAnnotations(
        resolveAnnotations([{ lines: "99", note: "a" }], 5),
      ),
    ).toBe(false);
  });
});

describe("rangeLabel", () => {
  it("labels a single line and a range", () => {
    const [single, range] = resolveAnnotations(
      [{ lines: "8", note: "a" }, { lines: "3-6", note: "b" }],
      10,
    );
    expect(rangeLabel(single)).toBe("Line 8");
    expect(rangeLabel(range)).toBe("Lines 3–6");
  });

  it("falls back to the raw ref for an unresolved annotation", () => {
    const [item] = resolveAnnotations([{ lines: "99", note: "a" }], 5);
    expect(rangeLabel(item)).toBe("Lines 99");
  });
});
