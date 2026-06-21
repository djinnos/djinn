import { describe, expect, it } from "vitest";

import {
  COLLAPSE_THRESHOLD,
  diffStats,
  isDiffLike,
  pairSplitRows,
  parseUnifiedDiff,
  segmentRows,
  splitFilename,
  type CollapsedRun,
  type DiffRow,
} from "./diff";

const SAMPLE = [
  "@@ -1,3 +1,4 @@",
  " export function add(a, b) {",
  "-  return a + b;",
  "+  const sum = a + b;",
  "+  return sum;",
  " }",
].join("\n");

describe("isDiffLike", () => {
  it("accepts a body with a hunk header", () => {
    expect(isDiffLike("@@ -1 +1 @@\n-a\n+b")).toBe(true);
  });

  it("accepts a headerless body with +/- lines", () => {
    expect(isDiffLike(" context\n-old\n+new")).toBe(true);
  });

  it("rejects plain prose", () => {
    expect(isDiffLike("This is just a sentence.\nNo diff here.")).toBe(false);
  });

  it("rejects an empty / whitespace body", () => {
    expect(isDiffLike("")).toBe(false);
    expect(isDiffLike("   \n  ")).toBe(false);
  });

  it("does not treat a bare `+++`/`---` file header as diff content", () => {
    // Only file headers, no actual body change lines and no hunk → not diff-like.
    expect(isDiffLike("--- a/x\n+++ b/x")).toBe(false);
  });
});

describe("parseUnifiedDiff — hunk parse + old/new numbering", () => {
  it("reconstructs old and new line numbers from the @@ header", () => {
    const rows = parseUnifiedDiff(SAMPLE);
    expect(rows.map((r) => [r.kind, r.oldNo, r.newNo, r.text])).toEqual([
      ["context", 1, 1, "export function add(a, b) {"],
      ["removed", 2, undefined, "  return a + b;"],
      ["added", undefined, 2, "  const sum = a + b;"],
      ["added", undefined, 3, "  return sum;"],
      ["context", 3, 4, "}"],
    ]);
  });

  it("strips file/index/rename header lines from the row stream", () => {
    const body = [
      "diff --git a/x.ts b/x.ts",
      "index 111..222 100644",
      "--- a/x.ts",
      "+++ b/x.ts",
      "@@ -1 +1 @@",
      "-old",
      "+new",
    ].join("\n");
    const rows = parseUnifiedDiff(body);
    expect(rows.map((r) => r.kind)).toEqual(["removed", "added"]);
    expect(rows[0].oldNo).toBe(1);
    expect(rows[1].newNo).toBe(1);
  });

  it("numbers a headerless diff from line 1", () => {
    const rows = parseUnifiedDiff(" a\n-b\n+c\n d");
    expect(rows.map((r) => [r.oldNo, r.newNo])).toEqual([
      [1, 1],
      [2, undefined],
      [undefined, 2],
      [3, 3],
    ]);
  });

  it("drops a `\\ No newline at end of file` marker", () => {
    const rows = parseUnifiedDiff("@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b");
    expect(rows.map((r) => r.kind)).toEqual(["removed", "added"]);
  });

  it("returns [] for empty input and never throws on garbage", () => {
    expect(parseUnifiedDiff("")).toEqual([]);
    expect(() => parseUnifiedDiff("@@ broken header @@\n??!!")).not.toThrow();
  });
});

describe("diffStats", () => {
  it("counts added and removed rows", () => {
    expect(diffStats(parseUnifiedDiff(SAMPLE))).toEqual({ added: 2, removed: 1 });
  });
});

describe("segmentRows — collapse-run logic", () => {
  function contextRows(count: number): DiffRow[] {
    return Array.from({ length: count }, (_, i) => ({
      kind: "context" as const,
      oldNo: i + 1,
      newNo: i + 1,
      text: `line ${i + 1}`,
    }));
  }

  it("does not collapse a short run", () => {
    const rows = contextRows(COLLAPSE_THRESHOLD);
    const segments = segmentRows(rows);
    expect(segments.every((s) => !("collapsed" in s))).toBe(true);
    expect(segments).toHaveLength(COLLAPSE_THRESHOLD);
  });

  it("collapses a long interior run keeping edge context", () => {
    const rows: DiffRow[] = [
      { kind: "removed", oldNo: 1, text: "x" },
      ...contextRows(20),
      { kind: "added", newNo: 22, text: "y" },
    ];
    const segments = segmentRows(rows);
    const collapsed = segments.find(
      (s): s is CollapsedRun => "collapsed" in s,
    );
    expect(collapsed).toBeDefined();
    // 20 context rows → 3 head + hidden + 3 tail.
    expect(collapsed!.rows.length).toBe(20 - 3 - 3);
    // The change rows stay visible at each end.
    expect(segments[0]).toMatchObject({ kind: "removed" });
    expect(segments[segments.length - 1]).toMatchObject({ kind: "added" });
  });

  it("collapses a long leading run to only the inner edge", () => {
    const rows: DiffRow[] = [
      ...contextRows(20),
      { kind: "added", newNo: 21, text: "y" },
    ];
    const segments = segmentRows(rows);
    // Leading run keeps no head context, only the inner tail edge + hidden.
    const visibleContext = segments.filter(
      (s) => !("collapsed" in s) && (s as DiffRow).kind === "context",
    );
    expect(visibleContext).toHaveLength(3);
  });

  it("never collapses changed rows", () => {
    const rows: DiffRow[] = Array.from({ length: 10 }, (_, i) => ({
      kind: "added" as const,
      newNo: i + 1,
      text: `+${i}`,
    }));
    const segments = segmentRows(rows);
    expect(segments.some((s) => "collapsed" in s)).toBe(false);
  });
});

describe("pairSplitRows — split reconstruction", () => {
  it("mirrors context rows on both columns", () => {
    const rows = parseUnifiedDiff(" a\n b");
    const pairs = pairSplitRows(rows);
    expect(pairs).toHaveLength(2);
    expect(pairs[0].left).toBe(pairs[0].right);
  });

  it("pairs a removed line with the corresponding added line", () => {
    const rows = parseUnifiedDiff("@@ -1 +1 @@\n-old\n+new");
    const pairs = pairSplitRows(rows);
    expect(pairs).toHaveLength(1);
    expect(pairs[0].left).toMatchObject({ kind: "removed", text: "old" });
    expect(pairs[0].right).toMatchObject({ kind: "added", text: "new" });
  });

  it("leaves half-empty cells for an unbalanced add", () => {
    // 1 removed, 2 added → 2 split rows, second has no left cell.
    const rows = parseUnifiedDiff("@@ -1 +1,2 @@\n-old\n+new1\n+new2");
    const pairs = pairSplitRows(rows);
    expect(pairs).toHaveLength(2);
    expect(pairs[0].left).toMatchObject({ text: "old" });
    expect(pairs[1].left).toBeUndefined();
    expect(pairs[1].right).toMatchObject({ text: "new2" });
  });
});

describe("splitFilename", () => {
  it("splits a path into basename + directory", () => {
    expect(splitFilename("src/routes/add.ts")).toEqual({
      basename: "add.ts",
      directory: "src/routes",
    });
  });

  it("returns no directory for a bare filename", () => {
    expect(splitFilename("add.ts")).toEqual({
      basename: "add.ts",
      directory: null,
    });
  });

  it("handles an absent filename", () => {
    expect(splitFilename(undefined)).toEqual({
      basename: "",
      directory: null,
    });
  });
});
