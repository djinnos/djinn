import { lazy, Suspense } from "react";

import type { BlockProps } from "./types";
import { BlockShell } from "./BlockShell";

// `react-syntax-highlighter` is heavy; load it only when a code block renders.
const CodeBlock = lazy(() => import("./CodeBlock"));

/**
 * AnnotatedCode — a syntax-highlighted code block with optional line notes.
 * Annotations come from the `annotations` attribute, a JSON string of
 * `{line, note}[]`, and render as small captions below the code. While the
 * highlighter chunk loads, the raw source shows in a plain `<pre>`.
 */
export function AnnotatedCode({ attributes, children }: BlockProps) {
  const language = attributes.lang ?? "text";
  const content = typeof children === "string" ? children.trim() : "";

  let annotations: { line: number; note: string }[] = [];
  if (attributes.annotations) {
    try {
      annotations = JSON.parse(attributes.annotations);
    } catch {
      // Invalid JSON — ignore annotations
    }
  }

  const rawCode = (
    <pre className="overflow-x-auto px-4 py-4 font-mono text-xs text-foreground">
      {content}
    </pre>
  );

  return (
    <BlockShell
      label="Code"
      accent="text-orange-400"
      flush
      meta={language !== "text" ? <span className="font-mono">{language}</span> : null}
    >
      <Suspense fallback={rawCode}>
        <CodeBlock language={language} content={content} />
      </Suspense>
      {annotations.length > 0 && (
        <div className="space-y-1 border-t px-4 py-3">
          {annotations.map((ann, i) => (
            <div
              key={i}
              className="flex items-start gap-2 text-xs text-muted-foreground"
            >
              <span className="shrink-0 rounded bg-muted px-1.5 py-0.5 font-mono font-medium text-foreground">
                L{ann.line}
              </span>
              <span>{ann.note}</span>
            </div>
          ))}
        </div>
      )}
    </BlockShell>
  );
}
