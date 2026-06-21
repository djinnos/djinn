import { lazy, Suspense } from "react";

import type { BlockProps } from "./types";
import { BlockShell } from "./BlockShell";

// Mermaid is a large dependency; load it only when a mermaid diagram renders.
const MermaidDiagram = lazy(() =>
  import("@/components/MermaidDiagram").then((m) => ({
    default: m.MermaidDiagram,
  })),
);

/**
 * Diagram — renders based on the `type` attribute:
 *   - `mermaid` (default): rendered to an actual SVG via the shared
 *     `MermaidDiagram` component (sandboxed + DOMPurify'd),
 *   - `svg`: inlined as-is,
 *   - anything else (e.g. `plantuml`, for which we have no client renderer):
 *     falls back to the raw source in a code block.
 */
export function Diagram({ attributes, children }: BlockProps) {
  const diagramType = attributes.type ?? "mermaid";
  const content = typeof children === "string" ? children.trim() : "";

  const rawSource = (
    <pre className="overflow-x-auto rounded-md bg-muted p-3 text-xs">
      <code>{content}</code>
    </pre>
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
          dangerouslySetInnerHTML={{ __html: content }}
        />
      ) : (
        rawSource
      )}
    </BlockShell>
  );
}
