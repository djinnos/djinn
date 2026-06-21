// Reusable React UI for the shared line-anchored annotation engine. The pure
// line-spec parsing + resolution lives in `annotations.ts`; the pure placement
// geometry + the hover-intent hook live in `annotationRail.helpers.ts`. This
// module owns ONLY the visible React components so the `annotated-code` block —
// and, later, the `diff` block and others — render line notes identically:
//
//  - `AnnotationGutterMarker` — the numbered amber pip placed on an annotated row.
//  - `AnnotationCard` — the single source of note-card markup (marker + line
//    label + optional emphasis + note), used by both the a11y hidden stack and
//    the on-hover popover.
//  - `AnnotationHiddenStack` — a visually-hidden (sr-only) stack of every note so
//    assistive tech and tests can reach the text without a persistent rail.
//  - `AnnotationHoverCard` — the on-hover portal popover, viewport-aware placed
//    beside the code (right → left → overlap → below).
//
// The app is DARK-ONLY. Notes carry plain-text bodies; this module renders them
// as text (no markdown) to stay dependency-light and avoid nested block parsing.

import { useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";

import { cn } from "@/lib/utils";

import { rangeLabel, type ResolvedAnnotation } from "./annotations";
import {
  HOVER_CARD_GAP,
  HOVER_CARD_WIDTH,
  resolveAnnotationHoverCardPosition,
  VIEWPORT_MARGIN,
  type AnnotationAnchor,
} from "./annotationRail.helpers";

/* ── Gutter marker ─────────────────────────────────────────────────────────── */

/**
 * The numbered amber pip rendered on an annotated code row's gutter. `active`
 * brightens it when its note (or a co-located row) is hovered/focused.
 */
export function AnnotationGutterMarker({
  marker,
  active,
  className,
}: {
  marker: number;
  active?: boolean;
  className?: string;
}) {
  return (
    <span
      aria-hidden
      className={cn(
        "inline-flex size-[15px] shrink-0 items-center justify-center rounded-full text-[9px] font-semibold leading-none tabular-nums transition-colors",
        active
          ? "bg-amber-300 text-amber-950 shadow-[0_0_0_3px_rgba(251,191,36,0.25)]"
          : "bg-amber-300/20 text-amber-200",
        className,
      )}
    >
      {marker}
    </span>
  );
}

/* ── Note card ─────────────────────────────────────────────────────────────── */

/**
 * One line-anchored note card: marker pip, the resolved line span ("Line 8"),
 * an optional label, and the plain-text note. Single source of card markup,
 * rendered both by the hidden a11y stack and the on-hover popover.
 */
export function AnnotationCard({
  item,
  active = false,
  className,
  onMouseEnter,
  onMouseLeave,
}: {
  item: ResolvedAnnotation;
  active?: boolean;
  className?: string;
  onMouseEnter?: () => void;
  onMouseLeave?: () => void;
}) {
  return (
    <div
      onMouseEnter={onMouseEnter}
      onMouseLeave={onMouseLeave}
      className={cn(
        "rounded-lg border px-3.5 py-2.5 text-left transition-colors",
        active
          ? "border-amber-300/40 bg-amber-300/10"
          : "border-border bg-card hover:border-amber-300/30",
        className,
      )}
    >
      <div className="flex min-w-0 flex-wrap items-baseline gap-x-2 gap-y-1">
        <AnnotationGutterMarker marker={item.marker} active={active} />
        <span className="shrink-0 text-[10px] font-semibold uppercase tracking-wide text-muted-foreground">
          {rangeLabel(item)}
        </span>
        {item.annotation.label && (
          <span className="min-w-0 max-w-full flex-1 break-words text-[13px] font-semibold leading-snug text-foreground [overflow-wrap:anywhere]">
            {item.annotation.label}
          </span>
        )}
      </div>
      <p className="mt-1 break-words text-[13px] leading-relaxed text-foreground/85 [overflow-wrap:anywhere]">
        {item.annotation.note}
      </p>
    </div>
  );
}

/**
 * Visually-hidden stack of every resolved note. Clipped to a 1px box (the
 * `sr-only` pattern) so the note text + marker pips stay in the accessibility
 * tree and the DOM (assistive tech can reach them, tests reading `textContent`
 * still see them) WITHOUT painting a persistent rail beside the code. The
 * visible card appears only on hover/focus via {@link AnnotationHoverCard}.
 */
export function AnnotationHiddenStack({
  items,
}: {
  items: readonly ResolvedAnnotation[];
}) {
  const resolved = useMemo(() => items.filter((item) => item.range), [items]);
  if (resolved.length === 0) return null;
  return (
    <div
      data-annotation-hidden-stack
      className="absolute size-px overflow-hidden whitespace-nowrap border-0 p-0 [clip:rect(0,0,0,0)] [clip-path:inset(50%)]"
    >
      {resolved.map((item) => (
        <AnnotationCard key={item.index} item={item} />
      ))}
    </div>
  );
}

/* ── Hover popover (portal, anchored beside the code) ──────────────────────── */

/**
 * The single on-hover note card, portaled to `document.body` and positioned
 * `fixed` so it escapes the code block's `overflow` and never reflows the code.
 * Keeps itself open while hovered (mouse events forwarded) and closes on scroll
 * so it never floats detached. Placement is measured one frame after mount (via
 * `requestAnimationFrame`) so the rendered card size feeds the viewport-aware
 * geometry without a synchronous setState-in-effect.
 */
export function AnnotationHoverCard({
  item,
  anchor,
  onMouseEnter,
  onMouseLeave,
  onClose,
}: {
  item: ResolvedAnnotation;
  anchor: AnnotationAnchor;
  onMouseEnter?: () => void;
  onMouseLeave?: () => void;
  onClose?: () => void;
}) {
  const cardRef = useRef<HTMLDivElement | null>(null);
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null);

  useEffect(() => {
    if (typeof window === "undefined") return;
    let frame: number | null = null;
    const measure = () => {
      frame = null;
      const rect = cardRef.current?.getBoundingClientRect();
      const width = rect && rect.width > 0 ? rect.width : HOVER_CARD_WIDTH;
      const height = rect && rect.height > 0 ? rect.height : 0;
      setPos(
        resolveAnnotationHoverCardPosition(
          anchor,
          { width, height },
          { width: window.innerWidth || 0, height: window.innerHeight || 0 },
        ),
      );
    };
    if (typeof window.requestAnimationFrame === "function") {
      frame = window.requestAnimationFrame(measure);
    } else {
      measure();
    }
    return () => {
      if (frame != null && typeof window.cancelAnimationFrame === "function") {
        window.cancelAnimationFrame(frame);
      }
    };
  }, [anchor]);

  useEffect(() => {
    if (!onClose || typeof window === "undefined") return;
    const handler = (event: Event) => {
      const target = event.target;
      if (
        target instanceof Node &&
        cardRef.current &&
        cardRef.current.contains(target)
      ) {
        return;
      }
      onClose();
    };
    window.addEventListener("scroll", handler, { capture: true, passive: true });
    return () =>
      window.removeEventListener("scroll", handler, { capture: true });
  }, [onClose]);

  if (typeof document === "undefined") return null;

  return createPortal(
    <div
      ref={cardRef}
      role="tooltip"
      data-annotation-hover-card
      onMouseEnter={onMouseEnter}
      onMouseLeave={onMouseLeave}
      className="pointer-events-auto fixed z-50 overflow-y-auto overscroll-contain"
      style={{
        top: pos?.top ?? anchor.lineCenter,
        left: pos?.left ?? anchor.codeRight + HOVER_CARD_GAP,
        width: HOVER_CARD_WIDTH,
        maxHeight: `calc(100vh - ${VIEWPORT_MARGIN * 2}px)`,
        visibility: pos ? "visible" : "hidden",
      }}
    >
      <AnnotationCard
        item={item}
        active
        className="shadow-lg shadow-black/40 backdrop-blur-md"
      />
    </div>,
    document.body,
  );
}
