import { describe, expect, it } from "vitest";
import { render, screen } from "@/test/test-utils";

import { DiffBlock } from "./DiffBlock";

const UNIFIED_DIFF = [
  "@@ -1,3 +1,4 @@",
  " export function add(a: number, b: number) {",
  "-  return a + b;",
  "+  const sum = a + b;",
  "+  return sum;",
  " }",
].join("\n");

describe("DiffBlock", () => {
  it("renders a unified diff with filename, +/- stats, and code lines", () => {
    render(
      <DiffBlock id="d1" attributes={{ filename: "src/add.ts", lang: "ts" }}>
        {UNIFIED_DIFF}
      </DiffBlock>,
    );
    // Header label + filename basename split.
    expect(screen.getByText("Diff")).toBeInTheDocument();
    expect(screen.getByText("add.ts")).toBeInTheDocument();
    expect(screen.getByText("src")).toBeInTheDocument();
    // +N / −N stats.
    expect(screen.getByText("+2")).toBeInTheDocument();
    expect(screen.getByText("−1")).toBeInTheDocument();
    // A removed and an added body line render.
    expect(screen.getByText("const sum = a + b;")).toBeInTheDocument();
    expect(screen.getByText("return a + b;")).toBeInTheDocument();
    // Both view toggles exist.
    expect(
      screen.getByRole("button", { name: "Unified" }),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Split" })).toBeInTheDocument();
  });

  it("falls back to a plain code block when children are not diff-like", () => {
    render(
      <DiffBlock id="d2" attributes={{}}>
        {"Just some prose, not a diff at all."}
      </DiffBlock>,
    );
    expect(
      screen.getByText("Just some prose, not a diff at all."),
    ).toBeInTheDocument();
    // No view-mode toggle in the fallback.
    expect(screen.queryByRole("button", { name: "Split" })).toBeNull();
  });

  it("renders a 'No changes' state for a diff hunk with only context", () => {
    render(
      <DiffBlock id="d3" attributes={{}}>
        {"@@ -1,2 +1,2 @@\n unchanged line one\n unchanged line two"}
      </DiffBlock>,
    );
    expect(screen.getByText("No changes")).toBeInTheDocument();
  });
});
