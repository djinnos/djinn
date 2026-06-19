import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import type { BlockProps } from "./types";

export function DataModelBlock({ id, children }: BlockProps) {
  return (
    <div id={id} className="rounded-lg border bg-card p-4 shadow-sm">
      <div className="mb-2 flex items-center justify-between gap-3">
        <span className="rounded-full bg-blue-100 px-2.5 py-0.5 text-xs font-semibold text-blue-800 dark:bg-blue-950 dark:text-blue-200">
          Data Model
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
