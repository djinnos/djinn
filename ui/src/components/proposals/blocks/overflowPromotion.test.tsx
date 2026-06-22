import { describe, expect, it } from "vitest";
import { render } from "@/test/test-utils";

import { TableBlock } from "./TableBlock";
import { FileTreeBlock } from "./FileTreeBlock";
import { DiffBlock } from "./DiffBlock";
import { JsonExplorerBlock } from "./JsonExplorerBlock";

/**
 * Regression: proposal block scroll containers used `overflow-x-auto` with no
 * explicit `overflow-y`. Per the CSS overflow spec a non-`visible` value on one
 * axis promotes the *other* axis to `auto`, so a 1px vertical content overrun
 * renders a stray VERTICAL scrollbar. On the proposal detail page this showed
 * up as a SECOND vertical scrollbar next to the page scrollbar. Every
 * horizontal-only container must clip vertically (`overflow-y-hidden`); the
 * PAGE owns vertical scroll.
 */
describe("block scroll containers never promote a stray vertical scrollbar", () => {
  function expectHorizontalOnly(el: Element | null | undefined) {
    expect(el).toBeTruthy();
    expect(el).toHaveClass("overflow-x-auto");
    expect(el).toHaveClass("overflow-y-hidden");
  }

  it("TableBlock wide-table wrapper clips vertically", () => {
    const body = [
      "| Name | Type | Notes |",
      "| ---- | ---- | ----- |",
      "| id   | uuid | primary key with a very long description column |",
    ].join("\n");
    const { container } = render(
      <TableBlock id="t-overflow" attributes={{}}>
        {body}
      </TableBlock>,
    );
    expectHorizontalOnly(container.querySelector(".overflow-x-auto"));
  });

  it("FileTreeBlock raw-fallback pre clips vertically", () => {
    // Unparseable body falls back to the raw monospace <pre>, the only
    // horizontal-scroll container in this block.
    const { container } = render(
      <FileTreeBlock id="ft-overflow" attributes={{}}>
        {"%%% not a file tree at all, just a very long single line %%%"}
      </FileTreeBlock>,
    );
    expectHorizontalOnly(container.querySelector("pre.overflow-x-auto"));
  });

  it("DiffBlock unified pre clips vertically", () => {
    const diff = [
      "@@ -1,2 +1,2 @@",
      "-const oldValueWithAVeryLongNameThatForcesHorizontalScroll = 1;",
      "+const newValueWithAVeryLongNameThatForcesHorizontalScroll = 2;",
    ].join("\n");
    const { container } = render(
      <DiffBlock id="d-overflow" attributes={{ filename: "x.ts", lang: "ts" }}>
        {diff}
      </DiffBlock>,
    );
    for (const el of container.querySelectorAll(".overflow-x-auto")) {
      expect(el).toHaveClass("overflow-y-hidden");
    }
    expect(
      container.querySelectorAll(".overflow-x-auto").length,
    ).toBeGreaterThan(0);
  });

  it("JsonExplorerBlock tree container clips vertically", () => {
    const { container } = render(
      <JsonExplorerBlock id="j-overflow" attributes={{}}>
        {'{ "id": "abc", "nested": { "a": 1, "b": 2 } }'}
      </JsonExplorerBlock>,
    );
    for (const el of container.querySelectorAll(".overflow-x-auto")) {
      expect(el).toHaveClass("overflow-y-hidden");
    }
    expect(
      container.querySelectorAll(".overflow-x-auto").length,
    ).toBeGreaterThan(0);
  });
});
