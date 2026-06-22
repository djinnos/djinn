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
 * Wide-diagram readability: the rendered SVG is wrapped in a zoom/pan surface
 * (`ZoomPanSurface`) — fit-to-width by default (previous behavior preserved),
 * with wheel/trackpad zoom, click-drag pan, zoom-in/out/reset controls, and an
 * optional fullscreen dialog (`allowFullscreen`, default on; `ImpactFlowModal`
 * omits it since it already lives in a Dialog).
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

import {
  memo,
  useCallback,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
} from "react";
import DOMPurify from "dompurify";
import {
  ArrowExpandIcon,
  ArrowReloadHorizontalIcon,
  Cancel01Icon,
  ZoomInAreaIcon,
  ZoomOutAreaIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
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
  return DOMPurify.sanitize(svg, SVG_SANITIZE_CONFIG);
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
}

interface RenderState {
  status: "idle" | "rendering" | "ready" | "error";
  svg?: string;
  error?: string;
}

const MIN_SCALE = 0.5;
const MAX_SCALE = 4;
const ZOOM_STEP = 1.25;

interface Transform {
  scale: number;
  x: number;
  y: number;
}

const IDENTITY: Transform = { scale: 1, x: 0, y: 0 };

function clampScale(s: number): number {
  return Math.min(MAX_SCALE, Math.max(MIN_SCALE, s));
}

/**
 * Zoom/pan surface around an already-rendered (sanitized) SVG. Fit-to-width by
 * default (the SVG keeps `max-w-full h-auto`), then a CSS `transform` (scale +
 * translate) layers interactive zoom on top — wheel/trackpad to zoom toward the
 * cursor, click-drag to pan when zoomed in. A tiny overlay exposes zoom-in,
 * zoom-out, reset(fit) and (optionally) fullscreen. Custom (no zoom/pan dep):
 * the whole behavior is one transform string + a handful of handlers.
 */
function ZoomPanSurface({
  svg,
  className,
  allowFullscreen = true,
  inFullscreen = false,
}: {
  svg: string;
  className?: string;
  allowFullscreen?: boolean;
  inFullscreen?: boolean;
}) {
  const [transform, setTransform] = useState<Transform>(IDENTITY);
  const [fullscreen, setFullscreen] = useState(false);
  // `dragging` drives the render-time transition toggle (refs can't be read in
  // render); `dragState` ref holds the live drag math, read only in handlers.
  const [dragging, setDragging] = useState(false);
  const viewportRef = useRef<HTMLDivElement | null>(null);
  const dragState = useRef<{
    startX: number;
    startY: number;
    originX: number;
    originY: number;
  } | null>(null);

  const reset = useCallback(() => setTransform(IDENTITY), []);

  // Zoom around the viewport center by a multiplicative factor.
  const zoomBy = useCallback((factor: number) => {
    setTransform((prev) => {
      const nextScale = clampScale(prev.scale * factor);
      if (nextScale === prev.scale) return prev;
      // Keep the content centered: scale about the viewport center, so the
      // existing translate is rescaled proportionally.
      const ratio = nextScale / prev.scale;
      return { scale: nextScale, x: prev.x * ratio, y: prev.y * ratio };
    });
  }, []);

  const onWheel = useCallback(
    (e: React.WheelEvent) => {
      // Only hijack the wheel for zoom (don't fight page scroll otherwise).
      if (!e.ctrlKey && !e.metaKey && Math.abs(e.deltaY) < 1) return;
      e.preventDefault();
      const viewport = viewportRef.current;
      if (!viewport) return;
      const rect = viewport.getBoundingClientRect();
      const cx = e.clientX - rect.left - rect.width / 2;
      const cy = e.clientY - rect.top - rect.height / 2;
      setTransform((prev) => {
        const dir = e.deltaY < 0 ? ZOOM_STEP : 1 / ZOOM_STEP;
        const nextScale = clampScale(prev.scale * dir);
        if (nextScale === prev.scale) return prev;
        const ratio = nextScale / prev.scale;
        // Zoom toward the cursor: the point under the cursor stays put.
        return {
          scale: nextScale,
          x: cx - (cx - prev.x) * ratio,
          y: cy - (cy - prev.y) * ratio,
        };
      });
    },
    [],
  );

  const onPointerDown = useCallback(
    (e: React.PointerEvent) => {
      if (transform.scale <= 1) return; // nothing to pan at fit
      (e.target as Element).setPointerCapture?.(e.pointerId);
      dragState.current = {
        startX: e.clientX,
        startY: e.clientY,
        originX: transform.x,
        originY: transform.y,
      };
      setDragging(true);
    },
    [transform],
  );

  const onPointerMove = useCallback((e: React.PointerEvent) => {
    const drag = dragState.current;
    if (!drag) return;
    setTransform((prev) => ({
      ...prev,
      x: drag.originX + (e.clientX - drag.startX),
      y: drag.originY + (e.clientY - drag.startY),
    }));
  }, []);

  const endDrag = useCallback(() => {
    dragState.current = null;
    setDragging(false);
  }, []);

  const zoomed = transform.scale > 1;

  const surface = (
    <div
      ref={viewportRef}
      data-testid="mermaid-zoompan-viewport"
      className={cn(
        "relative w-full overflow-hidden",
        zoomed ? "cursor-grab active:cursor-grabbing" : "cursor-default",
        inFullscreen ? "h-full" : "max-h-[70vh]",
      )}
      onWheel={onWheel}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={endDrag}
      onPointerLeave={endDrag}
    >
      <div
        data-testid="mermaid-zoompan-content"
        className="flex w-full items-center justify-center [&_svg]:h-auto [&_svg]:max-w-full"
        style={{
          transform: `translate(${transform.x}px, ${transform.y}px) scale(${transform.scale})`,
          transformOrigin: "center center",
          transition: dragging ? "none" : "transform 0.08s ease-out",
        }}
        // The svg is sanitized upstream; injecting via dangerouslySetInnerHTML
        // is the standard Mermaid integration pattern.
        dangerouslySetInnerHTML={{ __html: svg }}
      />

      {/* Controls overlay — top-right, compact ghost icon buttons. */}
      <div
        className="absolute right-2 top-2 flex items-center gap-1 rounded-md border border-border/60 bg-card/80 p-0.5 backdrop-blur"
        data-testid="mermaid-controls"
      >
        <Button
          type="button"
          variant="ghost"
          size="icon-sm"
          aria-label="Zoom in"
          title="Zoom in"
          onClick={() => zoomBy(ZOOM_STEP)}
        >
          <HugeiconsIcon icon={ZoomInAreaIcon} size={14} />
        </Button>
        <Button
          type="button"
          variant="ghost"
          size="icon-sm"
          aria-label="Zoom out"
          title="Zoom out"
          onClick={() => zoomBy(1 / ZOOM_STEP)}
        >
          <HugeiconsIcon icon={ZoomOutAreaIcon} size={14} />
        </Button>
        <Button
          type="button"
          variant="ghost"
          size="icon-sm"
          aria-label="Reset zoom"
          title="Reset to fit"
          onClick={reset}
        >
          <HugeiconsIcon icon={ArrowReloadHorizontalIcon} size={14} />
        </Button>
        {allowFullscreen && !inFullscreen && (
          <Button
            type="button"
            variant="ghost"
            size="icon-sm"
            aria-label="Fullscreen"
            title="Fullscreen"
            onClick={() => setFullscreen(true)}
          >
            <HugeiconsIcon icon={ArrowExpandIcon} size={14} />
          </Button>
        )}
        {inFullscreen && (
          <Button
            type="button"
            variant="ghost"
            size="icon-sm"
            aria-label="Exit fullscreen"
            title="Exit fullscreen"
            onClick={() => setFullscreen(false)}
          >
            <HugeiconsIcon icon={Cancel01Icon} size={14} />
          </Button>
        )}
      </div>
    </div>
  );

  return (
    <div className={cn("w-full", className)}>
      {surface}
      {allowFullscreen && !inFullscreen && (
        <Dialog open={fullscreen} onOpenChange={setFullscreen}>
          <DialogContent
            className="flex h-[90vh] max-w-[95vw] flex-col gap-2 sm:max-w-[95vw]"
            data-testid="mermaid-fullscreen"
          >
            <DialogHeader>
              <DialogTitle className="text-sm">Diagram</DialogTitle>
            </DialogHeader>
            <div className="min-h-0 flex-1 rounded-md border border-border/40">
              {/* A fresh ZoomPanSurface in fullscreen mode (its own transform
                  state, fills the dialog, no nested fullscreen button). */}
              <ZoomPanSurface
                svg={svg}
                allowFullscreen={false}
                inFullscreen
                className="h-full"
              />
            </div>
          </DialogContent>
        </Dialog>
      )}
    </div>
  );
}

function MermaidDiagramImpl({
  source,
  className,
  allowFullscreen = true,
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
      <ZoomPanSurface svg={state.svg} allowFullscreen={allowFullscreen} />
    </div>
  );
}

export const MermaidDiagram = memo(
  MermaidDiagramImpl,
  (prev, next) =>
    prev.source === next.source &&
    prev.className === next.className &&
    prev.allowFullscreen === next.allowFullscreen,
);
