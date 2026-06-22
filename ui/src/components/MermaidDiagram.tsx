/**
 * MermaidDiagram — reusable Mermaid renderer.
 *
 * Shared between `ImpactFlowModal` (blast-radius visualization) and the
 * proposal `Diagram` block, so a single change here covers every place we
 * render Mermaid.
 *
 * Renders via `beautiful-mermaid` (`renderMermaidSVG`): a pure-TS renderer that
 * emits native SVG `<text>` (not `<foreignObject>` HTML) with editor-grade
 * themes. Native `<text>` is the load-bearing win — it survives DOMPurify's
 * SVG-profile sanitize, so node labels can't be silently stripped (the failure
 * mode of the previous stock-mermaid `<foreignObject>` path).
 *
 * Theme: `tokyo-night` (`#1a1b26` bg) — chosen to sit on djinn's near-black
 * (`oklch(0.141 0.005 285.823)`) dark-only palette without reading as a bright
 * card on a dark page.
 *
 * Contract:
 *   - props: `{ source: string; className?: string }`,
 *   - normalizes unicode arrow/dash glyphs + auto-quotes node labels before
 *     render (LLM-authored sources frequently use `→`/`⟶`/`—` and unquoted
 *     special chars),
 *   - sanitizes the rendered SVG (DOMPurify, SVG profile) before injecting,
 *   - robust fallback chain so the block is never blank and never throws to the
 *     user:
 *       1. beautiful-mermaid (native `<text>`, themed) — primary,
 *       2. stock mermaid (`htmlLabels:false`, `theme:"dark"`) — secondary, for
 *          diagram types beautiful-mermaid doesn't fully support (e.g. some
 *          sequence diagrams),
 *       3. the raw source in a styled `<pre>` with a copy-source button.
 *
 * `beautiful-mermaid` pulls ELK.js (heavyish), so consumers lazy-load this
 * component (see `Diagram.tsx`'s `React.lazy` + `Suspense`) and the renderer is
 * `import()`ed inside the effect.
 *
 * Wrapped in `React.memo` so identical `source` strings don't re-trigger the
 * heavy SVG generation when parents re-render.
 */

import { memo, useEffect, useId, useMemo, useRef, useState } from "react";
import DOMPurify from "dompurify";

import { cn } from "@/lib/utils";
import { DiagramFallback } from "@/components/proposals/blocks/DiagramFallback";
import { normalizeMermaidSource } from "@/components/proposals/blocks/mermaidNormalize";

const SVG_SANITIZE_CONFIG = {
  USE_PROFILES: { svg: true, svgFilters: true },
} as const;

/**
 * Render via beautiful-mermaid (primary path). Dynamically imported so ELK.js
 * only loads when a diagram actually renders. Returns sanitized SVG markup.
 *
 * `renderMermaidSVG` is synchronous and takes `RenderOptions` shaped like the
 * theme's `DiagramColors` (bg/fg/line/accent/muted/…), so we spread the chosen
 * `THEMES[...]` entry directly into the options.
 */
async function renderWithBeautifulMermaid(source: string): Promise<string> {
  const { renderMermaidSVG, THEMES } = await import("beautiful-mermaid");
  const svg = renderMermaidSVG(source, {
    ...THEMES["tokyo-night"],
    // Defer to page CSS instead of pulling a renderer-specific font.
    font: "inherit",
  });
  return DOMPurify.sanitize(svg, SVG_SANITIZE_CONFIG);
}

let mermaidInitialized = false;
/**
 * Render via stock mermaid (secondary fallback). `htmlLabels:false` forces
 * native SVG `<text>` so labels survive the SVG-profile sanitize even on this
 * path; `theme:"dark"` matches the app's dark-only palette.
 */
async function renderWithStockMermaid(
  source: string,
  renderId: string,
): Promise<string> {
  const mermaid = (await import("mermaid")).default;
  if (!mermaidInitialized) {
    mermaid.initialize({
      startOnLoad: false,
      securityLevel: "strict",
      theme: "dark",
      htmlLabels: false,
      flowchart: { htmlLabels: false },
      fontFamily: "inherit",
    });
    mermaidInitialized = true;
  }
  const { svg } = await mermaid.render(renderId, source);
  return DOMPurify.sanitize(svg, SVG_SANITIZE_CONFIG);
}

export interface MermaidDiagramProps {
  /** Raw Mermaid source (e.g. `flowchart TD\n  a --> b`). */
  source: string;
  className?: string;
}

interface RenderState {
  status: "idle" | "rendering" | "ready" | "error";
  svg?: string;
  error?: string;
}

function MermaidDiagramImpl({ source, className }: MermaidDiagramProps) {
  // Stable id per instance — stock mermaid uses it as the SVG root id and
  // demands it be unique within the document and start with a letter.
  const reactId = useId();
  const renderId = `mermaid-${reactId.replace(/[^a-zA-Z0-9]/g, "")}`;

  const [state, setState] = useState<RenderState>({ status: "idle" });
  const containerRef = useRef<HTMLDivElement | null>(null);

  // Rewrite unicode arrow/dash glyphs to ASCII edge operators + auto-quote
  // node labels before render. Both renderers want valid mermaid grammar.
  const normalizedSource = useMemo(
    () => normalizeMermaidSource(source),
    [source],
  );

  useEffect(() => {
    let cancelled = false;
    setState({ status: "rendering" });

    (async () => {
      // Primary: beautiful-mermaid (native <text>, themed).
      try {
        const sanitized = await renderWithBeautifulMermaid(normalizedSource);
        if (cancelled) return;
        setState({ status: "ready", svg: sanitized });
        return;
      } catch (primaryErr) {
        // Secondary: stock mermaid (covers diagram types beautiful-mermaid
        // doesn't fully support).
        try {
          const sanitized = await renderWithStockMermaid(
            normalizedSource,
            renderId,
          );
          if (cancelled) return;
          setState({ status: "ready", svg: sanitized });
          return;
        } catch {
          if (cancelled) return;
          // Tertiary: copy-source fallback (handled by the error branch).
          setState({
            status: "error",
            error:
              primaryErr instanceof Error
                ? primaryErr.message
                : String(primaryErr),
          });
        }
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [normalizedSource, renderId]);

  if (state.status === "error") {
    // Never dump the raw parser exception or blank the block: show the original
    // source with a copy button and a calm one-line reason.
    return (
      <div className={className} data-testid="mermaid-error">
        <DiagramFallback
          source={source}
          message={`Could not render diagram: ${state.error}`}
        />
      </div>
    );
  }

  return (
    <div
      ref={containerRef}
      data-testid="mermaid-diagram"
      className={cn(
        // SVGs render unstyled by default — let parents control sizing via
        // `className`. Center horizontally so flowcharts don't hug the left
        // edge of a wide modal.
        "flex w-full items-center justify-center [&_svg]:max-w-full [&_svg]:h-auto",
        className,
      )}
      // The svg is sanitized above; injecting via dangerouslySetInnerHTML is the
      // standard Mermaid integration pattern.
      dangerouslySetInnerHTML={
        state.status === "ready" ? { __html: state.svg ?? "" } : undefined
      }
    >
      {state.status !== "ready" ? (
        <span className="text-xs text-muted-foreground">Rendering diagram…</span>
      ) : null}
    </div>
  );
}

export const MermaidDiagram = memo(
  MermaidDiagramImpl,
  (prev, next) =>
    prev.source === next.source && prev.className === next.className,
);
