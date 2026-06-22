import { describe, expect, it } from "vitest";

import { render, screen } from "@/test/test-utils";

import { WireframeBlock } from "./WireframeBlock";

describe("WireframeBlock", () => {
  it("renders the drawing as a monospace <pre> (no iframe, no svg)", () => {
    render(
      <WireframeBlock id="w1" attributes={{}}>
        {"┌────┐\n│ Hi │\n└────┘"}
      </WireframeBlock>,
    );
    const pre = screen.getByTestId("wireframe-ascii");
    expect(pre.tagName).toBe("PRE");
    expect(pre).toHaveTextContent("Hi");
    // Rendered as text, not an embedded iframe or a generated diagram <svg>.
    expect(document.querySelector("iframe")).toBeNull();
    expect(pre.querySelector("svg")).toBeNull();
  });

  it("uses the Monaspace Radon handwriting font", () => {
    render(
      <WireframeBlock id="w2" attributes={{}}>
        {"[ Save ]"}
      </WireframeBlock>,
    );
    const pre = screen.getByTestId("wireframe-ascii");
    expect(pre.getAttribute("style")).toContain("Monaspace Radon");
  });

  it("strips common leading indentation but keeps the box grid", () => {
    render(
      <WireframeBlock id="w3" attributes={{}}>
        {"    ┌──┐\n    │ok│\n    └──┘"}
      </WireframeBlock>,
    );
    // De-dented: lines start at the box edge, not four spaces in.
    expect(screen.getByTestId("wireframe-ascii").textContent).toBe(
      "┌──┐\n│ok│\n└──┘",
    );
  });

  it("offers a copy button", () => {
    render(
      <WireframeBlock id="w4" attributes={{}}>
        {"┌──┐\n└──┘"}
      </WireframeBlock>,
    );
    expect(
      screen.getByRole("button", { name: /copy wireframe/i }),
    ).toBeInTheDocument();
  });

  it("renders a calm placeholder for empty children without throwing", () => {
    expect(() =>
      render(
        <WireframeBlock id="w5" attributes={{ surface: "desktop" }}>
          {""}
        </WireframeBlock>,
      ),
    ).not.toThrow();
    expect(screen.getByTestId("wireframe-ascii")).toHaveTextContent(
      "empty wireframe",
    );
  });

  it("stays de-chromed (no shell label) and tolerates a legacy surface attr", () => {
    render(
      <WireframeBlock id="w6" attributes={{ surface: "mobile" }}>
        {"┌──┐\n│ok│\n└──┘"}
      </WireframeBlock>,
    );
    expect(screen.queryByText("Wireframe")).not.toBeInTheDocument();
    expect(screen.getByTestId("wireframe-ascii")).toBeInTheDocument();
  });
});
