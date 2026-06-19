import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import type { BlockProps } from "./types";

export function DecisionsBlock({ id, children }: BlockProps) {
  return (
    <div id={id} className="rounded-lg border bg-card p-4 shadow-sm">
      <div className="mb-2 flex items-center justify-between gap-3">
        <span className="rounded-full bg-amber-100 px-2.5 py-0.5 text-xs font-semibold text-amber-800 dark:bg-amber-950 dark:text-amber-200">
          Decisions
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
