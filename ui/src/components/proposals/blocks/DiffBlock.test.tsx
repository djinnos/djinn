import { beforeEach, describe, expect, it } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@/test/test-utils";

import { DiffBlock } from "./DiffBlock";

const UNIFIED_DIFF = [
  "@@ -1,3 +1,4 @@",
  " export function add(a: number, b: number) {",
  "-  return a + b;",
  "+  const sum = a + b;",
  "+  return sum;",
  " }",
].join("\n");

// A rust diff with a long unchanged run so collapse kicks in (> 6 context rows).
const RUST_DIFF = [
  "@@ -1,12 +1,12 @@",
  " fn run_chat_loop() {",
  "     let a = 1;",
  "     let b = 2;",
  "     let c = 3;",
  "     let d = 4;",
  "     let e = 5;",
  "     let f = 6;",
  "     let g = 7;",
  "-    let total = a + b;",
  "+    let total = a + b + c;",
  "     println!(\"{total}\");",
  " }",
].join("\n");

// Each test starts from a clean, default (unified) persisted view mode.
beforeEach(() => {
  window.localStorage.clear();
});

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

  it(
    "syntax-highlights the code with Prism tokens while keeping the +/- tint",
    { timeout: 20_000 },
    async () => {
      const { container } = render(
        <DiffBlock
          id="d4"
          attributes={{ filename: "src/loop.rs", lang: "rust" }}
        >
          {RUST_DIFF}
        </DiffBlock>,
      );

      // The Prism surface lazy-loads; once it does, code lines tokenize into
      // coloured `.token` spans (RSH inlines the oneDark colour onto each
      // token). The chunk (refractor + every grammar) is heavy, so give it a
      // generous window — under a fully loaded suite the import alone can
      // exceed the default 1s waitFor budget.
      const tokens = await waitFor(
        () => {
          const found = container.querySelectorAll("span.token");
          expect(found.length).toBeGreaterThan(0);
          return found;
        },
        { timeout: 15_000 },
      );
      // A `let` keyword token carries an inline colour — proof of highlighting.
      const keyword = Array.from(tokens).find((t) => t.textContent === "let");
      expect(keyword).toBeTruthy();
      expect((keyword as HTMLElement).style.color).not.toBe("");

      // The +/- tint still rides underneath: the added/removed rows keep their
      // low-alpha emerald/rose backgrounds (token colours show through them).
      expect(container.querySelector(".bg-emerald-500\\/10")).not.toBeNull();
      expect(container.querySelector(".bg-red-500\\/10")).not.toBeNull();

      // Stats still computed correctly (+1 / −1).
      expect(screen.getByText("+1")).toBeInTheDocument();
      expect(screen.getByText("−1")).toBeInTheDocument();
    },
  );

  it("keeps the collapsible unchanged run and toggle working under highlighting", () => {
    render(
      <DiffBlock id="d5" attributes={{ filename: "src/loop.rs", lang: "rust" }}>
        {RUST_DIFF}
      </DiffBlock>,
    );
    // The long unchanged run collapses behind an expandable control.
    const toggle = screen.getByRole("button", { name: /unchanged line/i });
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    fireEvent.click(toggle);
    expect(
      screen.getByRole("button", { name: /unchanged line/i }),
    ).toHaveAttribute("aria-expanded", "true");
  });

  it(
    "highlights both columns in split view (and the toggle persists)",
    { timeout: 20_000 },
    async () => {
      const { container } = render(
        <DiffBlock
          id="d6"
          attributes={{ filename: "src/loop.rs", lang: "rust" }}
        >
          {RUST_DIFF}
        </DiffBlock>,
      );
      // Switch to split view.
      fireEvent.click(screen.getByRole("button", { name: "Split" }));
      expect(screen.getByRole("button", { name: "Split" })).toHaveAttribute(
        "aria-pressed",
        "true",
      );
      // The choice is persisted to localStorage.
      expect(window.localStorage.getItem("djinn:proposal-diff-view-mode")).toBe(
        "split",
      );
      // Both side-by-side columns highlight (tokens present in each half).
      // Same generous window as the unified test — the Prism chunk lazy-loads.
      await waitFor(
        () => {
          const columns = container.querySelectorAll(".flex-1");
          expect(columns.length).toBeGreaterThanOrEqual(2);
          for (const col of [columns[0], columns[1]]) {
            expect(
              within(col as HTMLElement).getAllByText(
                (_, el) => el?.classList.contains("token") ?? false,
              ).length,
            ).toBeGreaterThan(0);
          }
        },
        { timeout: 15_000 },
      );
    },
  );

  it("falls back to plain (uncoloured) code when the language is unknown", async () => {
    const { container } = render(
      <DiffBlock id="d7" attributes={{ lang: "totally-unknown-lang" }}>
        {UNIFIED_DIFF}
      </DiffBlock>,
    );
    // The added line text renders intact as a single (untokenized) node — no
    // crash, just no colour.
    expect(screen.getByText("const sum = a + b;")).toBeInTheDocument();
    // Give the (no-op) lazy path a tick; still no token spans appear.
    await Promise.resolve();
    expect(container.querySelector("span.token")).toBeNull();
  });
});
