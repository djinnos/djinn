import { describe, expect, it } from "vitest";
import { render, screen } from "@/test/test-utils";

import { ColumnsBlock } from "./ColumnsBlock";
import { MAX_BLOCK_DEPTH } from "./blockDepth";

/** Build a valid `columns` JSON-attribute string from bodies. */
function columnsAttr(bodies: string[]): string {
  return JSON.stringify(bodies.map((body) => ({ body })));
}

describe("ColumnsBlock", () => {
  it("renders N columns, each with its recursively-rendered body", () => {
    const { container } = render(
      <ColumnsBlock
        id="c1"
        attributes={{
          columns: columnsAttr([
            "First column body",
            "Second column body",
            "Third column body",
          ]),
        }}
      />,
    );

    const grid = container.querySelector(".grid");
    expect(grid).not.toBeNull();
    // Three column cells.
    expect(grid!.children).toHaveLength(3);
    expect(screen.getByText("First column body")).toBeInTheDocument();
    expect(screen.getByText("Third column body")).toBeInTheDocument();
  });

  it("applies the responsive stack-on-narrow class", () => {
    const { container } = render(
      <ColumnsBlock
        id="c2"
        attributes={{ columns: columnsAttr(["A", "B"]) }}
      />,
    );

    const grid = container.querySelector(".grid");
    // Single column on narrow, two columns at md+ — the stack class is present.
    expect(grid!.className).toContain("grid-cols-1");
    expect(grid!.className).toContain("md:grid-cols-2");
  });

  it("recursively renders a child block inside a column (e.g. a Callout)", () => {
    render(
      <ColumnsBlock
        id="c3"
        attributes={{
          columns: columnsAttr([
            '<Callout id="x" tone="success">Looks good.</Callout>',
            "Plain other column",
          ]),
        }}
      />,
    );

    expect(screen.getByText("Success")).toBeInTheDocument();
    expect(screen.getByText("Looks good.")).toBeInTheDocument();
  });

  it("caps runaway nesting via the depth guard", () => {
    // Build columns nested one level deeper than the cap: at the cap the
    // innermost body is rendered as plain markdown, not parsed for more blocks,
    // so the number of nested grids is bounded (never infinite).
    const innermost = "DEEP_LEAF";
    let body = innermost;
    for (let i = 0; i < MAX_BLOCK_DEPTH + 3; i++) {
      body = `<Columns id="n${i}" columns={${columnsAttr([body])}} />`;
    }

    const { container } = render(
      <ColumnsBlock id="root" attributes={{ columns: columnsAttr([body]) }} />,
    );

    // The leaf content is still rendered (recursion terminated gracefully).
    expect(screen.getByText(/DEEP_LEAF/)).toBeInTheDocument();
    // The number of grids is bounded by the depth cap, not the nesting count.
    const grids = container.querySelectorAll(".grid");
    expect(grids.length).toBeLessThanOrEqual(MAX_BLOCK_DEPTH + 2);
  });

  it("falls back gracefully when the columns attribute is invalid JSON (no throw)", () => {
    expect(() =>
      render(
        <ColumnsBlock id="c4" attributes={{ columns: "[broken" }}>
          {"Fallback markdown body"}
        </ColumnsBlock>,
      ),
    ).not.toThrow();

    expect(screen.getByText("Fallback markdown body")).toBeInTheDocument();
  });

  it("shows a muted notice when invalid and no children", () => {
    render(<ColumnsBlock id="c5" attributes={{ columns: "nope" }} />);
    expect(screen.getByText("Could not render columns")).toBeInTheDocument();
  });
});
