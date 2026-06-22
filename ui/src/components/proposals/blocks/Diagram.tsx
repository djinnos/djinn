import { lazy, Suspense } from "react";

import type { BlockProps } from "./types";
import { BlockShell } from "./BlockShell";
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
export function Diagram({ attributes, children }: BlockProps) {
  const diagramType = attributes.type ?? "mermaid";
  const content = typeof children === "string" ? children.trim() : "";

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
    </BlockShell>
  );
}
