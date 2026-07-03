import { lazy, Suspense } from "react";

import type { BlockProps } from "./types";
import { unwrapExprValue } from "./exprAttr";
import { DiagramFallback } from "./DiagramFallback";
import { SandboxedHtmlFrame } from "./SandboxedHtmlFrame";

// Mermaid is a large dependency; load it only when a mermaid diagram renders.
const MermaidDiagram = lazy(() =>
  import("@/components/MermaidDiagram").then((m) => ({
    default: m.MermaidDiagram,
  })),
);

/**
 * Diagram — renders based on the `type` attribute:
 *   - `mermaid` (default): rendered to an actual SVG via the shared
 *     `MermaidDiagram` component (arrow-normalized + DOMPurify'd, with a
 *     graceful copy-source fallback on parse failure),
 *   - `svg`: rendered through the same sandboxed iframe surface as the `html`
 *     block (DOMPurify + `sandbox=""` + restrictive CSP) — NO raw
 *     `dangerouslySetInnerHTML`, closing the prior stored-XSS escape hatch,
 *   - anything else (e.g. `plantuml`, for which we have no client renderer):
 *     routed to the same graceful copy-source fallback instead of dumping raw.
 */
/** A diagram's source comes from the `source` schema field (preferred) or the
 *  block children. The `source` attribute is the form agents author with
 *  (`<Diagram type="mermaid" source={`…`} />`); reading children only — as this
 *  did before — dropped that source entirely and rendered an empty diagram. */
function diagramSource(
  source: string | undefined,
  children: BlockProps["children"],
): string {
  if (typeof source === "string" && source.trim().length > 0) {
    return unwrapExprValue(source).trim();
  }
  return typeof children === "string" ? children.trim() : "";
}

export function Diagram({ attributes, children }: BlockProps) {
  const diagramType = attributes.type ?? "mermaid";
  const content = diagramSource(attributes.source, children);

  // Guard: never render an empty/whitespace-only diagram. An empty source
  // renders as a broken "Empty mermaid diagram" box — drop the block instead.
  if (!content) return null;

  // De-chromed: a diagram should just BE there — no grey card, no "DIAGRAM"
  // header bar. It reads as a figure inside the document, the same way an image
  // or a heading-anchored section would.
  return (
    <figure className="my-1">
      {diagramType === "mermaid" ? (
        <Suspense
          fallback={
            <div className="py-6 text-center text-xs text-muted-foreground">
              Rendering diagram…
            </div>
          }
        >
          {/* Inline proposal diagrams are a STATIC overview: no inline
              pan/zoom, only a fullscreen button. Inspection (pan/zoom + all
              controls) happens in the fullscreen dialog. */}
          <MermaidDiagram source={content} inlineInteractive={false} />
        </Suspense>
      ) : diagramType === "svg" ? (
        // Author/agent SVG may carry `<script>`/`onload=`/`javascript:`
        // payloads. Render it through the shared sandboxed surface (DOMPurify +
        // locked-down iframe) instead of inlining it into the live DOM.
        <SandboxedHtmlFrame html={content} title="Diagram SVG" />
      ) : (
        // No client renderer for this type (e.g. plantuml): show the source with
        // a copy button rather than a raw dump.
        <DiagramFallback
          source={content}
          message={`No renderer for "${diagramType}" diagrams — showing source.`}
        />
      )}
    </figure>
  );
}
