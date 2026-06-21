import { lazy, Suspense, useMemo } from "react";
import DOMPurify from "dompurify";

import type { BlockProps } from "./types";
import { BlockShell } from "./BlockShell";
import { DiagramFallback } from "./DiagramFallback";

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
 *   - `svg`: inlined after DOMPurify sanitization (defends against stored XSS),
 *   - anything else (e.g. `plantuml`, for which we have no client renderer):
 *     routed to the same graceful copy-source fallback instead of dumping raw.
 */
export function Diagram({ attributes, children }: BlockProps) {
  const diagramType = attributes.type ?? "mermaid";
  const content = typeof children === "string" ? children.trim() : "";

  // Sanitize author/agent SVG before inlining — the source may carry
  // `<script>`/`onload=`/`javascript:` payloads. DOMPurify's SVG profile keeps
  // the drawing intact while stripping active markup.
  const sanitizedSvg = useMemo(
    () =>
      DOMPurify.sanitize(content, {
        USE_PROFILES: { svg: true, svgFilters: true, html: true },
      }),
    [content],
  );

  return (
    <BlockShell
      label="Diagram"
      accent="text-teal-400"
      meta={<span className="font-mono">{diagramType}</span>}
    >
      {diagramType === "mermaid" ? (
        <Suspense
          fallback={
            <div className="py-6 text-center text-xs text-muted-foreground">
              Rendering diagram…
            </div>
          }
        >
          <MermaidDiagram source={content} />
        </Suspense>
      ) : diagramType === "svg" ? (
        <div
          className="overflow-x-auto"
          // Content is DOMPurify-sanitized above before injection.
          dangerouslySetInnerHTML={{ __html: sanitizedSvg }}
        />
      ) : (
        // No client renderer for this type (e.g. plantuml): show the source with
        // a copy button rather than a raw dump.
        <DiagramFallback
          source={content}
          message={`No renderer for "${diagramType}" diagrams — showing source.`}
        />
      )}
    </BlockShell>
  );
}
