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
 * Theme: `DJINN_MERMAID_THEME` — a djinn-tokens-derived palette with a
 * TRANSPARENT background so the diagram BLENDS into the surrounding BlockShell
 * / card instead of painting an opaque band (the old `tokyo-night` `#1a1b26`
 * read as a lighter rectangle clashing with `--card`). Text is near-white,
 * accents/edges are the violet primary, node fills sit a hair above the card.
 * The stock-mermaid fallback is likewise forced transparent so it blends too.
 *
 * Wide-diagram readability: the rendered SVG is wrapped in the shared
 * `ZoomPanSurface` (extracted to `@/components/ZoomPanSurface`) — fit-to-width by
 * default (previous behavior preserved), with wheel/trackpad zoom, click-drag
 * pan, zoom-in/out/reset controls, and an optional fullscreen dialog
 * (`allowFullscreen`, default on; `ImpactFlowModal` omits it since it already
 * lives in a Dialog). The `mermaid-*` testid prefix and `"Diagram"` fullscreen
 * title are passed so the existing render specs keep matching.
 *
 * Contract:
 *   - props: `{ source: string; className?: string; allowFullscreen?: boolean }`,
 *   - normalizes unicode arrow/dash glyphs + auto-quotes node labels before
 *     render (LLM-authored sources frequently use `→`/`⟶`/`—` and unquoted
 *     special chars),
 *   - sanitizes the rendered SVG (DOMPurify, SVG profile) before injecting,
 *   - robust fallback chain so the block is never blank and never throws to the
 *     user:
 *       1. beautiful-mermaid (native `<text>`, themed) — primary,
 *       2. stock mermaid (`htmlLabels:false`, `theme:"dark"`, transparent bg) —
 *          secondary, for diagram types beautiful-mermaid doesn't fully support
 *          (e.g. some sequence diagrams),
 *       3. the raw source in a styled `<pre>` with a copy-source button.
 *
 * `beautiful-mermaid` pulls ELK.js (heavyish), so consumers lazy-load this
 * component (see `Diagram.tsx`'s `React.lazy` + `Suspense`) and the renderer is
 * `import()`ed inside the effect.
 *
 * Wrapped in `React.memo` so identical `source` strings don't re-trigger the
 * heavy SVG generation when parents re-render. The app is DARK-ONLY.
 */

import { memo, useEffect, useId, useMemo, useState } from "react";
import DOMPurify from "dompurify";

import { cn } from "@/lib/utils";
import { ZoomPanSurface } from "@/components/ZoomPanSurface";
import { DiagramFallback } from "@/components/proposals/blocks/DiagramFallback";
import { normalizeMermaidSource } from "@/components/proposals/blocks/mermaidNormalize";

const SVG_SANITIZE_CONFIG = {
  USE_PROFILES: { svg: true, svgFilters: true },
} as const;

/**
 * djinn-matched diagram palette, derived from the dark-only theme tokens in
 * `globals.css` and converted to sRGB hex (beautiful-mermaid wants concrete
 * colors). Keys map to `RenderOptions`/`DiagramColors`:
 *   bg      → background. We DON'T set it here; `transparent: true` is passed at
 *             the call site so the BlockShell/card shows through (the core fix).
 *             The value below is only a fallback for renderers that ignore
 *             `transparent` — it's `--card`, so worst case it still blends.
 *   fg      → primary text / node labels ≈ `--foreground` (near-white).
 *   muted   → secondary text / edge labels ≈ `--muted-foreground`.
 *   accent  → arrowheads / highlights = djinn VIOLET (`--primary`).
 *   line    → edges/connectors: a muted violet-gray with enough contrast.
 *   surface → node fill: a hair lighter than `--card`.
 *   border  → node/group stroke: violet-tinted border.
 */
export const DJINN_MERMAID_THEME = {
  bg: "#18181b", // --card (fallback only; we pass transparent:true)
  fg: "#ededed", // --foreground (near-white)
  muted: "#9f9fa9", // --muted-foreground
  accent: "#8650f6", // --primary (violet)
  line: "#55555b", // muted gray edges
  surface: "#1f1f22", // a hair lighter than --card
  border: "#584987", // violet-tinted border
} as const;

/**
 * Render via beautiful-mermaid (primary path). Dynamically imported so ELK.js
 * only loads when a diagram actually renders. Returns sanitized SVG markup.
 *
 * `renderMermaidSVG` is synchronous and takes `RenderOptions` shaped like the
 * theme's `DiagramColors` (bg/fg/line/accent/muted/surface/border), so we spread
 * the djinn theme directly and force `transparent` so the card shows through.
 */
async function renderWithBeautifulMermaid(source: string): Promise<string> {
  const { renderMermaidSVG } = await import("beautiful-mermaid");
  const svg = renderMermaidSVG(source, {
    ...DJINN_MERMAID_THEME,
    // The core blend fix: no opaque background rectangle on the SVG, so the
    // surrounding card paints through instead of a clashing band.
    transparent: true,
    // Defer to page CSS instead of pulling a renderer-specific font.
    font: "inherit",
  });
  // beautiful-mermaid unconditionally emits a Google Fonts @import for the
  // `font` name, so `font: "inherit"` yields a bogus `family=inherit` request
  // (CORS error in the console). Strip just that import — real font imports
  // (e.g. JetBrains Mono for `.mono`) are left alone.
  const withoutInheritFontImport = svg.replace(
    /@import url\('https:\/\/fonts\.googleapis\.com\/[^']*family=inherit[^']*'\);?/g,
    "",
  );
  return DOMPurify.sanitize(withoutInheritFontImport, SVG_SANITIZE_CONFIG);
}

let mermaidInitialized = false;
/**
 * Render via stock mermaid (secondary fallback). `htmlLabels:false` forces
 * native SVG `<text>` so labels survive the SVG-profile sanitize even on this
 * path; `theme:"dark"` matches the app's dark-only palette and a transparent
 * `themeVariables.background` keeps this path blending into the card too.
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
      // Don't inject mermaid's "Syntax error in text" bomb SVG into the
      // document body on parse failure — our own fallback handles errors.
      suppressErrorRendering: true,
      theme: "dark",
      htmlLabels: false,
      flowchart: { htmlLabels: false },
      fontFamily: "inherit",
      // Blend the fallback into the card: no opaque diagram background.
      themeVariables: {
        background: "transparent",
        primaryColor: DJINN_MERMAID_THEME.surface,
        primaryTextColor: DJINN_MERMAID_THEME.fg,
        primaryBorderColor: DJINN_MERMAID_THEME.border,
        lineColor: DJINN_MERMAID_THEME.line,
      },
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
  /**
   * Whether the zoom/pan controls include a fullscreen toggle. Default `true`
   * for the standalone proposal `Diagram` block; `ImpactFlowModal` sets this
   * `false` because it already renders inside a Dialog (avoids nested dialogs).
   */
  allowFullscreen?: boolean;
  /**
   * Whether the INLINE diagram is itself pan/zoom-interactive. Default `true`.
   * The proposal `Diagram` block sets this `false` so the inline diagram is a
   * STATIC overview (no wheel/drag, only a fullscreen button) — inspection
   * moves into the fullscreen dialog, which stays fully interactive.
   * `ImpactFlowModal` keeps the default `true` (it has no fullscreen to fall
   * back to, so inline must stay interactive).
   */
  inlineInteractive?: boolean;
}

interface RenderState {
  status: "idle" | "rendering" | "ready" | "error";
  svg?: string;
  error?: string;
}

/**
 * Render a sanitized SVG string as the zoomable child of `ZoomPanSurface`. The
 * svg is sanitized upstream; injecting via `dangerouslySetInnerHTML` is the
 * standard Mermaid integration pattern.
 */
function MermaidSvg({ svg }: { svg: string }) {
  return (
    <div
      className="contents"
      dangerouslySetInnerHTML={{ __html: svg }}
    />
  );
}

function MermaidDiagramImpl({
  source,
  className,
  allowFullscreen = true,
  inlineInteractive = true,
}: MermaidDiagramProps) {
  // Stable id per instance — stock mermaid uses it as the SVG root id and
  // demands it be unique within the document and start with a letter.
  const reactId = useId();
  const renderId = `mermaid-${reactId.replace(/[^a-zA-Z0-9]/g, "")}`;

  const [state, setState] = useState<RenderState>({ status: "idle" });

  // Rewrite unicode arrow/dash glyphs to ASCII edge operators + auto-quote
  // node labels before render. Both renderers want valid mermaid grammar.
  const normalizedSource = useMemo(
    () => normalizeMermaidSource(source),
    [source],
  );

  useEffect(() => {
    let cancelled = false;
    // eslint-disable-next-line react-hooks/set-state-in-effect -- async render pipeline: flip to the loading state before the off-thread mermaid render.
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

  if (state.status !== "ready" || !state.svg) {
    return (
      <div
        data-testid="mermaid-diagram"
        className={cn("flex w-full items-center justify-center", className)}
      >
        <span className="text-xs text-muted-foreground">Rendering diagram…</span>
      </div>
    );
  }

  return (
    <div data-testid="mermaid-diagram" className={cn("w-full", className)}>
      <ZoomPanSurface
        allowFullscreen={allowFullscreen}
        inlineInteractive={inlineInteractive}
        testIdPrefix="mermaid"
        fullscreenTitle="Diagram"
      >
        <MermaidSvg svg={state.svg} />
      </ZoomPanSurface>
    </div>
  );
}

export const MermaidDiagram = memo(
  MermaidDiagramImpl,
  (prev, next) =>
    prev.source === next.source &&
    prev.className === next.className &&
    prev.allowFullscreen === next.allowFullscreen &&
    prev.inlineInteractive === next.inlineInteractive,
);
