/**
 * ZoomPanSurface — a content-agnostic zoom/pan/fullscreen surface.
 *
 * Extracted from `MermaidDiagram` so a single, dependency-free implementation
 * backs every zoomable artboard: the Mermaid `Diagram` block (an SVG string) and
 * the `Wireframe` block (a sandboxed `<iframe>`). It takes arbitrary
 * `children` — whatever should be zoomed/panned — instead of an SVG string.
 *
 * Behavior (preserved verbatim from the Mermaid original):
 *   - fit-to-width by default (`scale: 1`); the content keeps its own sizing,
 *   - wheel/trackpad zoom TOWARD the cursor (the point under the cursor stays
 *     put),
 *   - click-drag pan once zoomed in past fit,
 *   - a compact top-right control cluster: zoom-in, zoom-out, reset(fit), and an
 *     optional fullscreen toggle that opens a fresh ZoomPanSurface in a Dialog.
 *
 * IFRAME WRINKLE (`framedContent`): a sandboxed `<iframe>` swallows pointer
 * events, so `pointerdown`/`pointermove` never reach the viewport and drag-pan
 * would be dead. When `framedContent` is set, while a drag is in progress we set
 * `pointer-events: none` on the content layer so the moves land on the viewport
 * instead — restoring drag-pan over a framed wireframe. Wheel-zoom already works
 * because the wheel event targets the viewport, not the iframe.
 *
 * Custom (no zoom/pan dep): the whole behavior is one transform string plus a
 * handful of handlers.
 */

import { useCallback, useRef, useState, type ReactNode } from "react";
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

export interface ZoomPanSurfaceProps {
  /** The content to zoom/pan (an SVG, an iframe, any node). */
  children: ReactNode;
  className?: string;
  /**
   * Whether the control cluster includes a fullscreen toggle. Default `true`.
   * Set `false` when the surface already lives inside a Dialog (avoids nested
   * dialogs) — the fullscreen copy passes this.
   */
  allowFullscreen?: boolean;
  /** Internal: this instance is the fullscreen copy (fills the dialog). */
  inFullscreen?: boolean;
  /**
   * Set when `children` is (or contains) an `<iframe>` that swallows pointer
   * events. While dragging, the content layer is made `pointer-events: none` so
   * pan moves reach the viewport. Default `false` (e.g. inline SVG).
   */
  framedContent?: boolean;
  /**
   * Prefix for the `data-testid`s on the viewport/content/controls/fullscreen
   * nodes. Defaults to `"zoompan"`. `MermaidDiagram` passes `"mermaid"` so its
   * existing render specs keep matching `mermaid-zoompan-*` / `mermaid-controls`.
   */
  testIdPrefix?: string;
  /** Title shown in the fullscreen dialog header. Default `"Preview"`. */
  fullscreenTitle?: string;
}

/**
 * Zoom/pan surface around arbitrary content. Fit-to-width by default, then a CSS
 * `transform` (scale + translate) layers interactive zoom on top.
 */
export function ZoomPanSurface({
  children,
  className,
  allowFullscreen = true,
  inFullscreen = false,
  framedContent = false,
  testIdPrefix = "zoompan",
  fullscreenTitle = "Preview",
}: ZoomPanSurfaceProps) {
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

  const onWheel = useCallback((e: React.WheelEvent) => {
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
  }, []);

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
      data-testid={`${testIdPrefix}-zoompan-viewport`}
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
        data-testid={`${testIdPrefix}-zoompan-content`}
        className="flex w-full items-center justify-center [&_svg]:h-auto [&_svg]:max-w-full"
        style={{
          transform: `translate(${transform.x}px, ${transform.y}px) scale(${transform.scale})`,
          transformOrigin: "center center",
          transition: dragging ? "none" : "transform 0.08s ease-out",
          // While dragging over a framed (iframe) child, let pointer moves fall
          // through to the viewport so drag-pan works (the iframe would
          // otherwise swallow them). Wheel-zoom is unaffected.
          ...(framedContent && dragging
            ? { pointerEvents: "none" as const }
            : {}),
        }}
      >
        {children}
      </div>

      {/* Controls overlay — top-right, compact ghost icon buttons. */}
      <div
        className="absolute right-2 top-2 flex items-center gap-1 rounded-md border border-border/60 bg-card/80 p-0.5 backdrop-blur"
        data-testid={`${testIdPrefix}-controls`}
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
            data-testid={`${testIdPrefix}-fullscreen`}
          >
            <DialogHeader>
              <DialogTitle className="text-sm">{fullscreenTitle}</DialogTitle>
            </DialogHeader>
            <div className="min-h-0 flex-1 rounded-md border border-border/40">
              {/* A fresh ZoomPanSurface in fullscreen mode (its own transform
                  state, fills the dialog, no nested fullscreen button). */}
              <ZoomPanSurface
                allowFullscreen={false}
                inFullscreen
                framedContent={framedContent}
                testIdPrefix={testIdPrefix}
                fullscreenTitle={fullscreenTitle}
                className="h-full"
              >
                {children}
              </ZoomPanSurface>
            </div>
          </DialogContent>
        </Dialog>
      )}
    </div>
  );
}
