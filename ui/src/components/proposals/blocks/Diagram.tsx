import type { BlockProps } from "./types";

/**
 * P1 block: Diagram — renders based on type attribute.
 * For mermaid/plantuml: renders a pre/code block with the source.
 * For svg: renders inline SVG.
 * Keep it simple for v1.
 */
export function Diagram({ id, attributes, children }: BlockProps) {
  const diagramType = attributes.type ?? "mermaid";
  const content = typeof children === "string" ? children.trim() : "";

  return (
    <div id={id} className="rounded-lg border bg-card p-4 shadow-sm">
      <div className="mb-2 flex items-center justify-between gap-3">
        <span className="rounded-full bg-teal-100 px-2.5 py-0.5 text-xs font-semibold text-teal-800 dark:bg-teal-950 dark:text-teal-200">
          Diagram
        </span>
        <div className="flex items-center gap-2">
          <span className="rounded bg-teal-50 px-2 py-0.5 font-mono text-xs text-teal-700 dark:bg-teal-950/60 dark:text-teal-300">
            {diagramType}
          </span>
          <span className="font-mono text-xs text-muted-foreground">{id}</span>
        </div>
      </div>
      {diagramType === "svg" ? (
        <div
          className="my-2 overflow-x-auto"
          dangerouslySetInnerHTML={{ __html: content }}
        />
      ) : (
        <pre className="my-2 overflow-x-auto rounded-md bg-muted p-3 text-xs">
          <code>{content}</code>
        </pre>
      )}
    </div>
  );
}
