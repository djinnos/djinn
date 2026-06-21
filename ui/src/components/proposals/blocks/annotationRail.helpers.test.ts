import { describe, expect, it } from "vitest";

import {
  resolveAnnotationHoverCardPosition,
  type AnnotationAnchor,
} from "./annotationRail.helpers";

const CARD = { width: 280, height: 100 };
const VIEWPORT = { width: 1200, height: 800 };

function anchor(partial: Partial<AnnotationAnchor>): AnnotationAnchor {
  return {
    codeRight: 600,
    codeLeft: 100,
    lineCenter: 400,
    lineBottom: 410,
    ...partial,
  };
}

describe("resolveAnnotationHoverCardPosition", () => {
  it("places the card to the right of the code when there is room", () => {
    const pos = resolveAnnotationHoverCardPosition(anchor({}), CARD, VIEWPORT);
    // right edge (600) + gap (12)
    expect(pos.left).toBe(612);
    // vertically centered on the line
    expect(pos.top).toBe(350);
  });

  it("falls back to the left when the right side would overflow", () => {
    const pos = resolveAnnotationHoverCardPosition(
      anchor({ codeRight: 1190, codeLeft: 700 }),
      CARD,
      VIEWPORT,
    );
    // left edge (700) - gap (12) - width (280)
    expect(pos.left).toBe(408);
  });

  it("never positions the card outside the viewport", () => {
    const pos = resolveAnnotationHoverCardPosition(
      anchor({ codeRight: 1190, codeLeft: 10, lineCenter: 5 }),
      CARD,
      VIEWPORT,
    );
    expect(pos.left).toBeGreaterThanOrEqual(8);
    expect(pos.left + CARD.width).toBeLessThanOrEqual(VIEWPORT.width - 8);
    expect(pos.top).toBeGreaterThanOrEqual(8);
  });
});
