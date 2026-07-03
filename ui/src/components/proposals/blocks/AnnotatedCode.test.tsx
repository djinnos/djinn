import { describe, expect, it } from "vitest";
import { render, screen } from "@/test/test-utils";

import { AnnotatedCode } from "./AnnotatedCode";

const CODE = ["const a = 1;", "const b = 2;", "return a + b;"].join("\n");

describe("AnnotatedCode", () => {
  it("renders the block shell, a filename header (dir/basename split), language switcher, and a copy button", () => {
    render(
      <AnnotatedCode
        id="c1"
        attributes={{ lang: "ts", filename: "src/utils/math.ts" }}
      >
        {CODE}
      </AnnotatedCode>,
    );
    expect(screen.getByText("Code")).toBeInTheDocument();
    expect(screen.getByText("math.ts")).toBeInTheDocument();
    expect(screen.getByText("src/utils/")).toBeInTheDocument();
    // The language switcher reflects the authored/inferred language via its
    // "Auto (…)" option.
    expect(
      screen.getByRole("combobox", { name: "Code language" }),
    ).toBeInTheDocument();
    expect(screen.getByText("Auto (ts)")).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Copy source" }),
    ).toBeInTheDocument();
  });

  it("renders a line-anchored annotation rail with a marker and the note text reachable", () => {
    render(
      <AnnotatedCode
        id="c2"
        attributes={{
          lang: "ts",
          annotations: JSON.stringify([
            { lines: "3", note: "the sum is returned here", label: "Return" },
          ]),
        }}
      >
        {CODE}
      </AnnotatedCode>,
    );
    // The optional label appears (footer rail + visually-hidden a11y stack).
    expect(screen.getAllByText("Return").length).toBeGreaterThan(0);
    // The marker pip number (1) is present.
    expect(screen.getAllByText("1").length).toBeGreaterThan(0);
    // The note text is reachable (visually-hidden a11y stack + Line label).
    expect(screen.getByText("Line 3")).toBeInTheDocument();
    expect(
      screen.getAllByText("the sum is returned here").length,
    ).toBeGreaterThan(0);
  });

  it("shows a warning and still renders the block when annotations JSON is invalid", () => {
    render(
      <AnnotatedCode id="c3" attributes={{ lang: "ts", annotations: "{bad json" }}>
        {CODE}
      </AnnotatedCode>,
    );
    expect(screen.getByText(/could not be parsed/i)).toBeInTheDocument();
    // The block still renders (no crash, code shell present).
    expect(screen.getByText("Code")).toBeInTheDocument();
  });

  it("renders without annotations and without a warning when none are provided", () => {
    render(
      <AnnotatedCode id="c4" attributes={{ lang: "ts" }}>
        {CODE}
      </AnnotatedCode>,
    );
    expect(screen.getByText("Code")).toBeInTheDocument();
    expect(screen.queryByText(/could not be parsed/i)).not.toBeInTheDocument();
  });

  it("renders code from the `code` expression attribute when authored self-closing (no children)", () => {
    // Regression: agents author `<AnnotatedCode code={`…`} />` (the block
    // catalog's `code` field — code with `<`/`{` cannot sit in children), and
    // the renderer read children only, so the block rendered empty.
    render(
      <AnnotatedCode
        id="c6"
        attributes={{
          language: "rust",
          code: "`pub struct Config {\n    pub url: Option<String>,\n}`",
        }}
      >
        {""}
      </AnnotatedCode>,
    );
    expect(screen.getByText(/pub struct Config/)).toBeInTheDocument();
    // The template-literal delimiters are stripped, not rendered.
    expect(screen.queryByText(/^`/)).not.toBeInTheDocument();
  });

  it("resolves annotations against attribute-sourced code lines", () => {
    render(
      <AnnotatedCode
        id="c7"
        attributes={{
          language: "ts",
          code: "`const a = 1;\nconst b = 2;\nreturn a + b;`",
          annotations: JSON.stringify([
            { line: "3", note: "the sum is returned here" },
          ]),
        }}
      >
        {""}
      </AnnotatedCode>,
    );
    expect(screen.getByText("Line 3")).toBeInTheDocument();
    expect(
      screen.getAllByText("the sum is returned here").length,
    ).toBeGreaterThan(0);
  });

  it("prefers the `code` attribute over children when both are present", () => {
    render(
      <AnnotatedCode id="c8" attributes={{ lang: "ts", code: '"from attr"' }}>
        {"from children"}
      </AnnotatedCode>,
    );
    expect(screen.getByText(/from attr/)).toBeInTheDocument();
    expect(screen.queryByText(/from children/)).not.toBeInTheDocument();
  });

  it("clips the code surface vertically so overflow-x never promotes a stray vertical scrollbar", () => {
    // Regression: the surface used only `overflow-x-auto` in its expanded
    // state, which the browser promotes the *other* axis to `auto`, rendering a
    // stray VERTICAL scrollbar on the proposal detail page. The surface is
    // content-height, so vertical clipping is a no-op except that it stops the
    // promotion. `overflow-y-hidden` must be present regardless of collapse.
    const { container } = render(
      <AnnotatedCode id="c5" attributes={{ lang: "ts" }}>
        {CODE}
      </AnnotatedCode>,
    );
    const surface = container.querySelector(".annotated-code-surface");
    expect(surface).toBeTruthy();
    expect(surface).toHaveClass("overflow-x-auto");
    expect(surface).toHaveClass("overflow-y-hidden");
  });
});
