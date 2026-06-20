import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import type { BlockProps } from "./types";

/**
 * P1 block: RichText — renders a content string styled with prose classes.
 * This is the simplest block type, essentially a styled markdown container.
 */
export function RichText({ id, children }: BlockProps) {
  return (
    <div id={id} className="rounded-lg border bg-card p-4 shadow-sm">
      <div className="mb-2 flex items-center justify-between gap-3">
        <span className="rounded-full bg-purple-100 px-2.5 py-0.5 text-xs font-semibold text-purple-800 dark:bg-purple-950 dark:text-purple-200">
          Rich Text
        </span>
        <span className="font-mono text-xs text-muted-foreground">{id}</span>
      </div>
      <div className="prose prose-sm max-w-none dark:prose-invert">
        {typeof children === "string" ? (
          <ReactMarkdown remarkPlugins={[remarkGfm]}>{children}</ReactMarkdown>
        ) : (
          children
        )}
      </div>
    </div>
  );
}
