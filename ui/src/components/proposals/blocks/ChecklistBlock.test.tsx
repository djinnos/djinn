import { describe, expect, it } from "vitest";
import { render } from "@/test/test-utils";

import { ChecklistBlock } from "./ChecklistBlock";

const BODY = ["- [x] Ship it", "- [ ] Verify it"].join("\n");

/**
 * The unchecked marker previously used `Square01Icon`, whose hugeicons-3.3.0
 * artwork is NOT a square — it draws an "x²"/chi-squared glyph (two diagonal
 * strokes + a superscript-2 hook). We switched to `SquareIcon` (a plain rounded
 * outline) for unchecked and keep `CheckmarkSquare02Icon` for checked. These
 * specs lock in the rendered SVG path data so the math-glyph can't regress.
 */
describe("ChecklistBlock — marker glyphs", () => {
  it("renders an empty square outline for unchecked and a checkmark for checked", () => {
    const { container } = render(
      <ChecklistBlock id="c1" attributes={{}}>
        {BODY}
      </ChecklistBlock>,
    );

    const items = container.querySelectorAll("li");
    expect(items).toHaveLength(2);

    const dOf = (li: Element) =>
      [...li.querySelectorAll("svg path")].map((p) => p.getAttribute("d") ?? "");

    // The rounded-square outline path shared by SquareIcon / CheckmarkSquare02.
    const SQUARE_OUTLINE = "M2.5 12C2.5 7.52166";
    const CHECK_TICK = "M8 12.5L10.5 15L16 9";

    const checkedPaths = dOf(items[0]);
    const uncheckedPaths = dOf(items[1]);

    // Checked: rounded square + a tick.
    expect(checkedPaths.some((d) => d.startsWith(SQUARE_OUTLINE))).toBe(true);
    expect(checkedPaths).toContain(CHECK_TICK);

    // Unchecked: a single rounded-square outline, NO tick.
    expect(uncheckedPaths.some((d) => d.startsWith(SQUARE_OUTLINE))).toBe(true);
    expect(uncheckedPaths).not.toContain(CHECK_TICK);

    // Crucially: the old broken Square01 "x²" path must be gone everywhere.
    const allPaths = [...checkedPaths, ...uncheckedPaths];
    expect(allPaths.some((d) => d.startsWith("M2.71474 7.02474"))).toBe(false);
  });

  it("keeps the done/total tally and line-through on checked items", () => {
    const { getByText, container } = render(
      <ChecklistBlock id="c2" attributes={{}}>
        {BODY}
      </ChecklistBlock>,
    );

    expect(getByText("1/2 done")).toBeInTheDocument();

    const checkedLabel = getByText("Ship it");
    expect(checkedLabel.className).toContain("line-through");

    // Read-only: no interactive checkboxes/inputs are rendered.
    expect(container.querySelectorAll("input")).toHaveLength(0);
  });
});
