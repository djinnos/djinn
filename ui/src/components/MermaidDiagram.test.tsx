import { describe, expect, it } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { renderMermaidSVG } from "beautiful-mermaid";

import { DJINN_MERMAID_THEME, MermaidDiagram } from "./MermaidDiagram";
import { normalizeMermaidSource } from "@/components/proposals/blocks/mermaidNormalize";

// The real repro from the task: a `graph TD` with unicode arrows and node
// labels carrying parentheses / `-D` / `--check` special chars — exactly the
// LLM-authored shape that the previous `<foreignObject>` path silently blanked.
const REPRO_SOURCE = `graph TD
  PR[PR push] ⟶ A[clippy -D warnings (exists)]
  PR ⟶ B[hakari verify (exists)]
  PR ⟶ C[cargo-deny: advisories + licenses (NEW)]
  PR ⟶ D[cargo-deny bans: sqlx wrapper-allowlist ratchet (NEW)]
  PR ⟶ E[check-boundaries OR grep guard: no raw SQL outside djinn-db (NEW)]
  PR ⟶ F[sqlx prepare --check on every PR, not just merge_group (MOVED)]`;

const REPRO_LABELS = [
  "PR push",
  "clippy -D warnings (exists)",
  "hakari verify (exists)",
  "cargo-deny: advisories + licenses (NEW)",
  "cargo-deny bans: sqlx wrapper-allowlist ratchet (NEW)",
  "check-boundaries OR grep guard: no raw SQL outside djinn-db (NEW)",
  "sqlx prepare --check on every PR, not just merge_group (MOVED)",
];

describe("beautiful-mermaid render contract", () => {
  it("emits native SVG <text> labels (no <foreignObject>) for the normalized repro", () => {
    const normalized = normalizeMermaidSource(REPRO_SOURCE);
    const svg = renderMermaidSVG(normalized, {
      ...DJINN_MERMAID_THEME,
      transparent: true,
      font: "inherit",
    });

    // The whole point: real <text> elements, never the HTML-in-SVG
    // <foreignObject> wrapper that the SVG-profile sanitize strips.
    expect(svg).toContain("<text");
    expect(svg).not.toContain("foreignObject");

    // Every node label is present as visible text.
    for (const label of REPRO_LABELS) {
      expect(svg).toContain(label);
    }
  });

  it("renders transparent (no opaque bg rect) so it blends into the card", () => {
    const svg = renderMermaidSVG("graph TD\n  A[a] --> B[b]", {
      ...DJINN_MERMAID_THEME,
      transparent: true,
    });
    // The blend fix: with transparent:true the renderer must NOT paint the
    // old tokyo-night band — and the djinn violet accent should be present.
    expect(svg.toLowerCase()).not.toContain("#1a1b26");
    expect(svg.toLowerCase()).toContain(DJINN_MERMAID_THEME.accent.toLowerCase());
  });
});

describe("MermaidDiagram", () => {
  it("renders the repro to an SVG with visible labels and no error fallback", async () => {
    render(<MermaidDiagram source={REPRO_SOURCE} />);

    const container = await screen.findByTestId("mermaid-diagram");

    await waitFor(() => {
      expect(container.querySelector("svg")).not.toBeNull();
    });

    // Did NOT fall through to the copy-source error fallback.
    expect(screen.queryByTestId("mermaid-error")).toBeNull();

    const svg = container.querySelector("svg")!;
    expect(svg.querySelectorAll("text").length).toBeGreaterThan(0);
    expect(container.textContent).toContain("PR push");
    expect(container.textContent).toContain("cargo-deny: advisories + licenses (NEW)");
  });

  it("falls back to the copy-source surface on unrenderable input", async () => {
    // Empty/blank source can't produce a diagram on either renderer → the
    // graceful tertiary fallback, never a thrown exception or a blank block.
    render(<MermaidDiagram source={"   "} />);

    const fallback = await screen.findByTestId("mermaid-error");
    expect(fallback).not.toBeNull();
  });
});

describe("MermaidDiagram — zoom/pan controls", () => {
  function scaleOf(el: HTMLElement): number {
    const t = el.style.transform;
    const m = /scale\(([\d.]+)\)/.exec(t);
    return m ? Number(m[1]) : 1;
  }

  it("exposes zoom-in/out/reset controls; zoom-in scales up and reset restores", async () => {
    render(<MermaidDiagram source={REPRO_SOURCE} />);
    await screen.findByTestId("mermaid-diagram");
    await waitFor(() =>
      expect(screen.getByTestId("mermaid-controls")).toBeInTheDocument(),
    );

    const content = screen.getByTestId("mermaid-zoompan-content");
    expect(scaleOf(content)).toBe(1); // fit-to-width default

    const zoomIn = screen.getByRole("button", { name: "Zoom in" });
    const zoomOut = screen.getByRole("button", { name: "Zoom out" });
    const reset = screen.getByRole("button", { name: "Reset zoom" });
    expect(zoomIn).toBeInTheDocument();
    expect(zoomOut).toBeInTheDocument();
    expect(reset).toBeInTheDocument();

    fireEvent.click(zoomIn);
    await waitFor(() => expect(scaleOf(content)).toBeGreaterThan(1));

    fireEvent.click(reset);
    await waitFor(() => expect(scaleOf(content)).toBe(1));
  });

  it("shows a fullscreen toggle by default and hides it when allowFullscreen=false", async () => {
    const { rerender } = render(<MermaidDiagram source={REPRO_SOURCE} />);
    await screen.findByTestId("mermaid-diagram");
    await waitFor(() =>
      expect(
        screen.getByRole("button", { name: "Fullscreen" }),
      ).toBeInTheDocument(),
    );

    rerender(<MermaidDiagram source={REPRO_SOURCE} allowFullscreen={false} />);
    await waitFor(() =>
      expect(
        screen.queryByRole("button", { name: "Fullscreen" }),
      ).toBeNull(),
    );
  });
});
