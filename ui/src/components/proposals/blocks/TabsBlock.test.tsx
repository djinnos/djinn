import { describe, expect, it } from "vitest";
import { render, screen, userEvent } from "@/test/test-utils";

import { TabsBlock } from "./TabsBlock";

/** Build a valid `tabs` JSON-attribute string from entries. */
function tabsAttr(entries: { label: string; body: string }[]): string {
  return JSON.stringify(entries);
}

describe("TabsBlock", () => {
  it("renders one tab button per entry and shows the first tab by default", () => {
    render(
      <TabsBlock
        id="t1"
        attributes={{
          tabs: tabsAttr([
            { label: "Overview", body: "First panel body" },
            { label: "API", body: "Second panel body" },
            { label: "Notes", body: "Third panel body" },
          ]),
        }}
      />,
    );

    const tabButtons = screen.getAllByRole("tab");
    expect(tabButtons).toHaveLength(3);
    expect(tabButtons[0]).toHaveAttribute("aria-selected", "true");
    expect(tabButtons[1]).toHaveAttribute("aria-selected", "false");

    // First panel is visible.
    expect(screen.getByText("First panel body")).toBeInTheDocument();
  });

  it("switches the active panel when a tab is clicked", async () => {
    const user = userEvent.setup();
    render(
      <TabsBlock
        id="t2"
        attributes={{
          tabs: tabsAttr([
            { label: "One", body: "Panel one" },
            { label: "Two", body: "Panel two" },
          ]),
        }}
      />,
    );

    await user.click(screen.getByRole("tab", { name: "Two" }));

    expect(screen.getByRole("tab", { name: "Two" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
    expect(screen.getByText("Panel two")).toBeInTheDocument();
  });

  it("moves the active tab with arrow keys (keyboard accessible)", async () => {
    const user = userEvent.setup();
    render(
      <TabsBlock
        id="t3"
        attributes={{
          tabs: tabsAttr([
            { label: "Alpha", body: "Alpha body" },
            { label: "Beta", body: "Beta body" },
          ]),
        }}
      />,
    );

    const first = screen.getByRole("tab", { name: "Alpha" });
    first.focus();
    await user.keyboard("{ArrowRight}");

    expect(screen.getByRole("tab", { name: "Beta" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
    expect(screen.getByText("Beta body")).toBeInTheDocument();
  });

  it("recursively renders a child block inside a tab (e.g. a Callout)", () => {
    render(
      <TabsBlock
        id="t4"
        attributes={{
          tabs: tabsAttr([
            {
              label: "WithCallout",
              body: '<Callout id="c1" tone="warning">Be careful here.</Callout>',
            },
          ]),
        }}
      />,
    );

    // The recursively-rendered Callout shows its tone label and body.
    expect(screen.getByText("Warning")).toBeInTheDocument();
    expect(screen.getByText("Be careful here.")).toBeInTheDocument();
  });

  it("never lets the tab-header strip scroll vertically (overflow-y hidden)", () => {
    // Regression: the tablist previously used only `overflow-x-auto`, which the
    // browser promotes the *other* axis to `auto`, so the buttons' `-mb-px` +
    // `border-b-2` overran the strip by 1px and rendered a stray VERTICAL
    // scrollbar. The header must clip vertically.
    render(
      <TabsBlock
        id="tscroll"
        attributes={{
          tabs: tabsAttr([
            { label: "Overview", body: "a" },
            { label: "Details", body: "b" },
            { label: "Risks", body: "c" },
          ]),
        }}
      />,
    );

    const tablist = screen.getByRole("tablist");
    expect(tablist).toHaveClass("overflow-y-hidden");
    expect(tablist).toHaveClass("overflow-x-auto");
  });

  it("falls back gracefully when the tabs attribute is invalid JSON (no throw)", () => {
    expect(() =>
      render(
        <TabsBlock id="t5" attributes={{ tabs: "{not valid json" }}>
          {"Fallback markdown body"}
        </TabsBlock>,
      ),
    ).not.toThrow();

    expect(screen.queryAllByRole("tab")).toHaveLength(0);
    expect(screen.getByText("Fallback markdown body")).toBeInTheDocument();
  });

  it("shows a muted notice with the raw attribute when invalid and no children", () => {
    render(<TabsBlock id="t6" attributes={{ tabs: "nope" }} />);
    expect(screen.getByText("Could not render tabs")).toBeInTheDocument();
    expect(screen.getByText("nope")).toBeInTheDocument();
  });
});
