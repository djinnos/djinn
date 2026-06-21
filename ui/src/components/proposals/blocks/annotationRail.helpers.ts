// Pure (React-free) geometry + the hover-intent controller hook for the shared
// line-anchored annotation engine. Split out of `annotationRail.tsx` so that
// module can export ONLY React components (Fast-Refresh requirement) while the
// viewport-aware placement math stays unit-testable and reusable.

import { useEffect, useRef, useState } from "react";

/** The geometry the hover card anchors to (in viewport coordinates). */
export interface AnnotationAnchor {
  /** Right edge of the code block (where the card should start). */
  codeRight: number;
  /** Left edge of the code block (for the below-fallback alignment). */
  codeLeft: number;
  /** Vertical center of the hovered line. */
  lineCenter: number;
  /** Bottom of the hovered line (for the below-fallback placement). */
  lineBottom: number;
}

export type AnnotationSide = "left" | "right";

export const HOVER_CARD_WIDTH = 280;
export const HOVER_CARD_GAP = 12;
const HOVER_CARD_OVERHANG = 40;
export const VIEWPORT_MARGIN = 8;

function oppositeSide(side: AnnotationSide): AnnotationSide {
  return side === "left" ? "right" : "left";
}

function clampWithinViewport(
  value: number,
  size: number,
  viewportSize: number,
): number {
  return Math.max(
    VIEWPORT_MARGIN,
    Math.min(value, viewportSize - size - VIEWPORT_MARGIN),
  );
}

function hoverCardLeftForSide(
  side: AnnotationSide,
  anchor: AnnotationAnchor,
  cardWidth: number,
): number {
  return side === "right"
    ? anchor.codeRight + HOVER_CARD_GAP
    : anchor.codeLeft - HOVER_CARD_GAP - cardWidth;
}

function hoverCardOverlapLeftForSide(
  side: AnnotationSide,
  anchor: AnnotationAnchor,
  cardWidth: number,
): number {
  return side === "right"
    ? anchor.codeRight - cardWidth + HOVER_CARD_OVERHANG
    : anchor.codeLeft - HOVER_CARD_OVERHANG;
}

function hoverCardFitsSide(
  side: AnnotationSide,
  anchor: AnnotationAnchor,
  cardWidth: number,
  viewportWidth: number,
): boolean {
  const left = hoverCardLeftForSide(side, anchor, cardWidth);
  return (
    left >= VIEWPORT_MARGIN &&
    left + cardWidth + VIEWPORT_MARGIN <= viewportWidth
  );
}

/**
 * Resolve the non-overlapping placement for the hover card: to the RIGHT of the
 * code if it fits, then the opposite side, then overlapping the code from the
 * fallback side, otherwise BELOW the line aligned to the code's left. Clamped
 * within the viewport so the card is never cut off. Pure → unit-testable.
 */
export function resolveAnnotationHoverCardPosition(
  anchor: AnnotationAnchor,
  card: { width: number; height: number },
  viewport: { width: number; height: number },
  options: {
    preferredSide?: AnnotationSide;
    hoverFallbackSide?: AnnotationSide | "below";
  } = {},
): { top: number; left: number } {
  const preferredSide = options.preferredSide ?? "right";
  const hoverFallbackSide = options.hoverFallbackSide ?? "right";
  const opposite = oppositeSide(preferredSide);

  let left: number;
  let top: number;
  if (hoverCardFitsSide(preferredSide, anchor, card.width, viewport.width)) {
    left = hoverCardLeftForSide(preferredSide, anchor, card.width);
    top = anchor.lineCenter - card.height / 2;
  } else if (hoverCardFitsSide(opposite, anchor, card.width, viewport.width)) {
    left = hoverCardLeftForSide(opposite, anchor, card.width);
    top = anchor.lineCenter - card.height / 2;
  } else if (hoverFallbackSide === "left" || hoverFallbackSide === "right") {
    left = hoverCardOverlapLeftForSide(hoverFallbackSide, anchor, card.width);
    top = anchor.lineCenter - card.height / 2;
  } else {
    left = anchor.codeLeft;
    top = anchor.lineBottom + HOVER_CARD_GAP;
  }

  left = clampWithinViewport(left, card.width, viewport.width);
  top = Math.max(
    VIEWPORT_MARGIN,
    Math.min(top, viewport.height - card.height - VIEWPORT_MARGIN),
  );
  return { top, left };
}

/**
 * Build an {@link AnnotationAnchor} from the code block element and the hovered
 * row element. The card anchors to the code block's RIGHT edge and the row's
 * vertical center, both in viewport coordinates (so a `fixed` portal lines up).
 */
export function anchorFromElements(
  codeEl: HTMLElement | null,
  rowEl: HTMLElement | null,
): AnnotationAnchor | null {
  if (!codeEl || !rowEl) return null;
  const code = codeEl.getBoundingClientRect();
  const row = rowEl.getBoundingClientRect();
  return {
    codeRight: code.right,
    codeLeft: code.left,
    lineCenter: row.top + row.height / 2,
    lineBottom: row.bottom,
  };
}

/**
 * Hover-intent + tap controller for the on-hover note card. Exposes the active
 * index + captured `anchor`, plus `open`/`toggle`/`close`/`scheduleClose`/
 * `cancelClose` handlers. The short close delay lets the pointer cross the gap
 * from the code line into the card itself without it disappearing.
 */
export function useAnnotationHover(delay = 130) {
  const [active, setActive] = useState<{
    index: number;
    anchor: AnnotationAnchor;
  } | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const cancelClose = () => {
    if (timer.current) {
      clearTimeout(timer.current);
      timer.current = null;
    }
  };
  const open = (index: number, anchor: AnnotationAnchor) => {
    cancelClose();
    setActive({ index, anchor });
  };
  const toggle = (index: number, anchor: AnnotationAnchor) => {
    cancelClose();
    setActive((prev) => (prev?.index === index ? null : { index, anchor }));
  };
  const close = () => {
    cancelClose();
    setActive(null);
  };
  const scheduleClose = () => {
    cancelClose();
    timer.current = setTimeout(() => setActive(null), delay);
  };

  useEffect(() => () => cancelClose(), []);

  return {
    activeIndex: active?.index ?? null,
    anchor: active?.anchor ?? null,
    open,
    toggle,
    close,
    scheduleClose,
    cancelClose,
  } as const;
}
