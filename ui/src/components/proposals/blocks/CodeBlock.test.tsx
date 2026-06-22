import { describe, expect, it } from "vitest";
import { render } from "@/test/test-utils";

import CodeBlock from "./CodeBlock";

// The deny.toml repro from BUG C: with line numbers on, each logical source
// line (including the `[advisories]` / `[licenses]` / `[bans]` section headers)
// must render as exactly ONE row, with its number in the same row box.
const TOML = [
  "# server/deny.toml",
  "[advisories]",
  'yanked = "deny"',
  "[licenses]",
  'allow = ["MIT", "Apache-2.0"]',
  "[bans]",
  'multiple-versions = "warn"',
].join("\n");

describe("CodeBlock — line-number alignment", () => {
  it("renders exactly one row per logical line with inline numbers", () => {
    const { container } = render(
      <CodeBlock language="toml" content={TOML} showLineNumbers />,
    );

    // Inline line numbers live INSIDE each row (one per line), not in a
    // separate floated gutter <code>.
    const numbers = container.querySelectorAll(
      ".react-syntax-highlighter-line-number",
    );
    expect(numbers.length).toBe(7);

    // Numbers are 1..7 in order.
    const texts = Array.from(numbers).map((n) => n.textContent?.trim());
    expect(texts).toEqual(["1", "2", "3", "4", "5", "6", "7"]);

    // Each row is a block-level, non-wrapping element so a line can never split
    // or drift out of alignment with its number.
    const code = container.querySelector("code");
    expect(code).not.toBeNull();
    const rows = Array.from(code!.children).filter(
      (el) =>
        el.querySelector(".react-syntax-highlighter-line-number") !== null,
    );
    expect(rows.length).toBe(7);
    for (const row of rows) {
      const style = (row as HTMLElement).style;
      expect(style.display).toBe("block");
      expect(style.whiteSpace).toBe("pre");
    }
  });

  it("keeps each section header on its own row (no split)", () => {
    const { container } = render(
      <CodeBlock language="toml" content={TOML} showLineNumbers />,
    );
    const code = container.querySelector("code")!;
    const rows = Array.from(code.children).filter(
      (el) =>
        el.querySelector(".react-syntax-highlighter-line-number") !== null,
    );
    // Row 2 (index 1) is the `[advisories]` section header — its row text
    // contains the full bracketed header and nothing from another line.
    const row2Text = rows[1].textContent ?? "";
    expect(row2Text).toContain("[advisories]");
    expect(row2Text).not.toContain("yanked");
  });

  it("carries the .djinn-code collision-guard hook on its surface", () => {
    // The Prism `[section]` table-header token ships as `<span class="token
    // table">`, which collides with Tailwind's global `.table { display: table }`
    // utility and splits the row (BUG C). The fix is a `.djinn-code .token {
    // display: inline }` guard in globals.css; jsdom can't compute the Tailwind
    // layout, but it CAN assert the anchor class stays wired to the surface so
    // the CSS guard keeps matching. Also assert the colliding token exists.
    const { container } = render(
      <CodeBlock language="toml" content={TOML} showLineNumbers />,
    );
    expect(container.querySelector(".djinn-code")).not.toBeNull();
    // The `[advisories]` header tokenized to a `.token.table` span — the exact
    // class that collides with Tailwind's `.table` utility.
    expect(container.querySelector(".token.table")).not.toBeNull();
  });

  it("applies per-line props from lineProps to the matching row", () => {
    const { container } = render(
      <CodeBlock
        language="toml"
        content={TOML}
        showLineNumbers
        lineProps={(n) =>
          n === 2 ? { "data-annotated": "true" } : { className: "plain-line" }
        }
      />,
    );
    const annotated = container.querySelectorAll('[data-annotated="true"]');
    expect(annotated.length).toBe(1);
    // The annotated row is line 2 → contains the `[advisories]` header.
    expect(annotated[0].textContent).toContain("[advisories]");
  });
});
