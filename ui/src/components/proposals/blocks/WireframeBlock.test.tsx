import { describe, expect, it } from "vitest";
import { render, screen } from "@/test/test-utils";

import { WireframeBlock } from "./WireframeBlock";
import { WIREFRAME_SURFACES } from "./wireframeHtml";

function frame(): HTMLIFrameElement {
  return screen.getByTitle("Wireframe mockup") as HTMLIFrameElement;
}

describe("WireframeBlock", () => {
  it("renders the children inside a fully sandboxed iframe (no scripts, no same-origin)", () => {
    render(
      <WireframeBlock id="w1" attributes={{ surface: "desktop" }}>
        {'<div class="wf-card"><h1>Sign in</h1></div>'}
      </WireframeBlock>,
    );
    const iframe = frame();
    expect(iframe.tagName).toBe("IFRAME");
    expect(iframe.getAttribute("sandbox")).toBe("");
    expect(iframe.getAttribute("sandbox")).not.toContain("allow-scripts");
    const srcdoc = iframe.getAttribute("srcdoc") ?? "";
    expect(srcdoc).toContain("--wf-ink:");
    expect(srcdoc).toContain('<div class="wf-card">');
    expect(srcdoc).toContain("Sign in");
    expect(srcdoc).toContain("Content-Security-Policy");
  });

  it("applies the surface preset width (default desktop)", () => {
    const { rerender } = render(
      <WireframeBlock id="w2" attributes={{ surface: "mobile" }}>
        {"<p>hi</p>"}
      </WireframeBlock>,
    );
    expect(frame().style.width).toBe(`${WIREFRAME_SURFACES.mobile.width}px`);

    // Missing/unknown surface falls back to the default desktop footprint.
    rerender(
      <WireframeBlock id="w2" attributes={{}}>
        {"<p>hi</p>"}
      </WireframeBlock>,
    );
    expect(frame().style.width).toBe(`${WIREFRAME_SURFACES.desktop.width}px`);
  });

  it("transforms data-icon markers into inline svg in the srcdoc", () => {
    render(
      <WireframeBlock id="w3" attributes={{ surface: "browser" }}>
        {'<button><span data-icon="search"></span> Search</button>'}
      </WireframeBlock>,
    );
    const srcdoc = frame().getAttribute("srcdoc") ?? "";
    expect(srcdoc).toContain("<svg");
    expect(srcdoc).toContain('class="wf-icon"');
    expect(srcdoc).toContain('data-icon="search"');
  });

  it("strips active markup (scripts) from the wireframe srcdoc", () => {
    render(
      <WireframeBlock id="w4" attributes={{ surface: "desktop" }}>
        {"<b>keep</b><script>alert(1)</script>"}
      </WireframeBlock>,
    );
    const srcdoc = frame().getAttribute("srcdoc") ?? "";
    expect(srcdoc).toContain("<b>keep</b>");
    expect(srcdoc).not.toContain("alert(1)");
  });

  it("exposes zoom/pan controls (de-chromed: no shell label)", () => {
    render(
      <WireframeBlock id="w5" attributes={{ surface: "desktop" }}>
        {"<p>body</p>"}
      </WireframeBlock>,
    );
    // No "Wireframe" header/eyebrow — the mockup's own device frame is the chrome.
    expect(screen.queryByText("Wireframe")).not.toBeInTheDocument();
    expect(screen.getByTestId("wireframe-controls")).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Zoom in" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Reset zoom" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Fullscreen" }),
    ).toBeInTheDocument();
  });

  it("renders a calm frame for empty/invalid children without throwing", () => {
    expect(() =>
      render(
        <WireframeBlock id="w6" attributes={{ surface: "desktop" }}>
          {""}
        </WireframeBlock>,
      ),
    ).not.toThrow();
    expect(frame().tagName).toBe("IFRAME");
  });
});
