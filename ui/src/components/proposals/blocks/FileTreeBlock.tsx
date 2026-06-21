import type { BlockProps } from "./types";
import { BlockShell } from "./BlockShell";

/**
 * FileTree — renders the tree source as preformatted monospace text. We
 * deliberately do NOT run it through markdown: markdown collapses the single
 * newlines that give a file tree its shape into one run-on paragraph, which is
 * exactly the "where's my tree?" bug. `whitespace-pre` keeps the author's
 * indentation and line breaks intact.
 */
export function FileTreeBlock({ attributes, children }: BlockProps) {
  const root = attributes.root;
  const content = typeof children === "string" ? children.trim() : "";

  return (
    <BlockShell
      label="Files"
      accent="text-slate-400"
      flush
      meta={root ? <span className="truncate font-mono">{root}</span> : null}
    >
      <pre className="overflow-x-auto px-4 py-3 font-mono text-xs leading-relaxed text-foreground">
        {content}
      </pre>
    </BlockShell>
  );
}
