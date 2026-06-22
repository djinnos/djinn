import { describe, expect, it } from "vitest";
import { fireEvent, render, screen } from "@/test/test-utils";

import { ZoomPanSurface } from "./ZoomPanSurface";

function scaleOf(el: HTMLElement): number {
  const m = /scale\(([\d.]+)\)/.exec(el.style.transform);
  return m ? Number(m[1]) : 1;
}

describe("ZoomPanSurface", () => {
  it("renders arbitrary children and the zoom controls (fit-to-width by default)", () => {
    render(
      <ZoomPanSurface testIdPrefix="wf">
        <div data-testid="payload">hello</div>
      </ZoomPanSurface>,
    );
    expect(screen.getByTestId("payload")).toBeInTheDocument();
    expect(screen.getByTestId("wf-controls")).toBeInTheDocument();
    const content = screen.getByTestId("wf-zoompan-content");
    expect(scaleOf(content)).toBe(1);
  });

  it("zoom-in scales up and reset restores fit", () => {
    render(
      <ZoomPanSurface testIdPrefix="wf">
        <div>x</div>
      </ZoomPanSurface>,
    );
    const content = screen.getByTestId("wf-zoompan-content");
    fireEvent.click(screen.getByRole("button", { name: "Zoom in" }));
    expect(scaleOf(content)).toBeGreaterThan(1);
    fireEvent.click(screen.getByRole("button", { name: "Reset zoom" }));
    expect(scaleOf(content)).toBe(1);
  });

  it("hides the fullscreen toggle when allowFullscreen=false", () => {
    const { rerender } = render(
      <ZoomPanSurface testIdPrefix="wf">
        <div>x</div>
      </ZoomPanSurface>,
    );
    expect(
      screen.getByRole("button", { name: "Fullscreen" }),
    ).toBeInTheDocument();

    rerender(
      <ZoomPanSurface testIdPrefix="wf" allowFullscreen={false}>
        <div>x</div>
      </ZoomPanSurface>,
    );
    expect(
      screen.queryByRole("button", { name: "Fullscreen" }),
    ).toBeNull();
  });

  it("framedContent makes the content layer pointer-events:none while dragging", () => {
    render(
      <ZoomPanSurface testIdPrefix="wf" framedContent>
        <iframe title="framed" />
      </ZoomPanSurface>,
    );
    const viewport = screen.getByTestId("wf-zoompan-viewport");
    const content = screen.getByTestId("wf-zoompan-content");

    // At fit (scale 1) there is nothing to pan, so no drag layer engages.
    expect(content.style.pointerEvents).toBe("");

    // Zoom in so a drag can start, then begin a drag: the content layer goes
    // pointer-events:none so moves reach the viewport over the iframe.
    fireEvent.click(screen.getByRole("button", { name: "Zoom in" }));
    fireEvent.pointerDown(viewport, { clientX: 10, clientY: 10, pointerId: 1 });
    expect(content.style.pointerEvents).toBe("none");

    fireEvent.pointerUp(viewport, { pointerId: 1 });
    expect(content.style.pointerEvents).toBe("");
  });

  describe("static inline mode (inlineInteractive=false)", () => {
    it("shows ONLY the fullscreen control (no zoom-in/out/reset)", () => {
      render(
        <ZoomPanSurface testIdPrefix="wf" inlineInteractive={false}>
          <div>x</div>
        </ZoomPanSurface>,
      );
      // The overlay still renders, but only the fullscreen toggle is in it.
      expect(
        screen.getByRole("button", { name: "Fullscreen" }),
      ).toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "Zoom in" })).toBeNull();
      expect(screen.queryByRole("button", { name: "Zoom out" })).toBeNull();
      expect(screen.queryByRole("button", { name: "Reset zoom" })).toBeNull();
    });

    it("does NOT wheel-zoom or drag-pan the inline viewport", () => {
      render(
        <ZoomPanSurface testIdPrefix="wf" inlineInteractive={false}>
          <div>x</div>
        </ZoomPanSurface>,
      );
      const viewport = screen.getByTestId("wf-zoompan-viewport");
      const content = screen.getByTestId("wf-zoompan-content");
      expect(scaleOf(content)).toBe(1);

      // Wheel does nothing (no handler attached) — scale stays at fit.
      fireEvent.wheel(viewport, { deltaY: -200, ctrlKey: true });
      expect(scaleOf(content)).toBe(1);

      // Drag does nothing either — no pan translate is applied.
      fireEvent.pointerDown(viewport, {
        clientX: 10,
        clientY: 10,
        pointerId: 1,
      });
      fireEvent.pointerMove(viewport, {
        clientX: 80,
        clientY: 80,
        pointerId: 1,
      });
      fireEvent.pointerUp(viewport, { pointerId: 1 });
      expect(content.style.transform).not.toMatch(/translate\((?!0px, 0px)/);

      // The static viewport must not clip/capture scroll.
      expect(viewport.className).toContain("overflow-visible");
      expect(viewport.className).not.toContain("overflow-hidden");
    });

    it("static + no fullscreen renders no empty controls overlay", () => {
      render(
        <ZoomPanSurface
          testIdPrefix="wf"
          inlineInteractive={false}
          allowFullscreen={false}
        >
          <div>x</div>
        </ZoomPanSurface>,
      );
      expect(screen.queryByTestId("wf-controls")).toBeNull();
    });
  });

  it("uses the testIdPrefix for all surface nodes", () => {
    render(
      <ZoomPanSurface testIdPrefix="mermaid">
        <div>x</div>
      </ZoomPanSurface>,
    );
    expect(screen.getByTestId("mermaid-zoompan-viewport")).toBeInTheDocument();
    expect(screen.getByTestId("mermaid-zoompan-content")).toBeInTheDocument();
    expect(screen.getByTestId("mermaid-controls")).toBeInTheDocument();
  });
});
