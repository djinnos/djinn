import { describe, expect, it } from "vitest";
import { render, screen } from "@/test/test-utils";

import { AnnotatedCode } from "./AnnotatedCode";

const CODE = ["const a = 1;", "const b = 2;", "return a + b;"].join("\n");

describe("AnnotatedCode", () => {
  it("renders the block shell, a filename header (dir/basename split), language chip, and a copy button", () => {
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
    expect(screen.getByText("ts")).toBeInTheDocument();
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
});
