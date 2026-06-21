import { describe, expect, it } from "vitest";

import { parseTable } from "./table";

describe("parseTable — GFM tables", () => {
  it("parses a header + separator + body rows", () => {
    const body = [
      "| Name  | Type   | Notes        |",
      "| ----- | ------ | ------------ |",
      "| id    | uuid   | primary key  |",
      "| email | string | unique       |",
    ].join("\n");

    const table = parseTable(body);
    expect(table).not.toBeNull();
    expect(table!.header).toEqual(["Name", "Type", "Notes"]);
    expect(table!.rows).toEqual([
      ["id", "uuid", "primary key"],
      ["email", "string", "unique"],
    ]);
  });

  it("skips the alignment separator row (`:--:` / `---`)", () => {
    const body = ["| A | B |", "| :-- | --: |", "| 1 | 2 |"].join("\n");
    const table = parseTable(body)!;
    expect(table.rows).toEqual([["1", "2"]]);
  });

  it("tolerates missing leading/trailing pipes", () => {
    const body = ["A | B", "1 | 2"].join("\n");
    const table = parseTable(body)!;
    expect(table.header).toEqual(["A", "B"]);
    expect(table.rows).toEqual([["1", "2"]]);
  });

  it("normalizes ragged rows to the header width", () => {
    const body = ["| A | B | C |", "| 1 | 2 |", "| 1 | 2 | 3 | 4 |"].join("\n");
    const table = parseTable(body)!;
    expect(table.rows).toEqual([
      ["1", "2", ""],
      ["1", "2", "3"],
    ]);
  });

  it("preserves escaped pipes inside a cell", () => {
    const body = ["| Expr | Meaning |", "| --- | --- |", "| a \\| b | or |"].join(
      "\n",
    );
    const table = parseTable(body)!;
    expect(table.rows[0]).toEqual(["a | b", "or"]);
  });
});

describe("parseTable — fallback", () => {
  it("returns null for non-table prose (no pipes)", () => {
    expect(parseTable("Just a sentence.")).toBeNull();
    expect(parseTable("")).toBeNull();
    expect(parseTable("   \n  ")).toBeNull();
  });

  it("returns null when the only line is an empty header", () => {
    expect(parseTable("|  |  |")).toBeNull();
  });

  it("parses a header-only table with no body rows", () => {
    const table = parseTable("| A | B |\n| --- | --- |")!;
    expect(table.header).toEqual(["A", "B"]);
    expect(table.rows).toEqual([]);
  });
});
