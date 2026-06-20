import type { BlockProps } from "./types";

/**
 * P1 block: AnnotatedCode — renders a code block with optional line annotations.
 * For v1, render the code in a pre/code block and show annotations as small
 * notes below. Annotations are parsed from the `annotations` attribute, which
 * is expected to be a JSON string of `{line, note}[]`.
 */
export function AnnotatedCode({ id, attributes, children }: BlockProps) {
  const language = attributes.lang ?? "";
  const content = typeof children === "string" ? children.trim() : "";

  let annotations: { line: number; note: string }[] = [];
  if (attributes.annotations) {
    try {
      annotations = JSON.parse(attributes.annotations);
    } catch {
      // Invalid JSON — ignore annotations
    }
  }

  return (
    <div id={id} className="rounded-lg border bg-card p-4 shadow-sm">
      <div className="mb-2 flex items-center justify-between gap-3">
        <span className="rounded-full bg-orange-100 px-2.5 py-0.5 text-xs font-semibold text-orange-800 dark:bg-orange-950 dark:text-orange-200">
          Annotated Code
        </span>
        <div className="flex items-center gap-2">
          {language && (
            <span className="rounded bg-orange-50 px-2 py-0.5 font-mono text-xs text-orange-700 dark:bg-orange-950/60 dark:text-orange-300">
              {language}
            </span>
          )}
          <span className="font-mono text-xs text-muted-foreground">{id}</span>
        </div>
      </div>
      <pre className="my-2 overflow-x-auto rounded-md bg-muted p-3 text-xs">
        <code>{content}</code>
      </pre>
      {annotations.length > 0 && (
        <div className="mt-2 space-y-1">
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
    </div>
  );
}
