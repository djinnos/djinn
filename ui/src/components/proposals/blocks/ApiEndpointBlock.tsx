import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import type { BlockProps } from "./types";

export function ApiEndpointBlock({ id, attributes, children }: BlockProps) {
  const method = attributes.method?.toUpperCase();
  const path = attributes.path;

  return (
    <div id={id} className="rounded-lg border bg-card p-4 shadow-sm">
      <div className="mb-2 flex items-center justify-between gap-3">
        <span className="rounded-full bg-green-100 px-2.5 py-0.5 text-xs font-semibold text-green-800 dark:bg-green-950 dark:text-green-200">
          API Endpoint
        </span>
        <span className="font-mono text-xs text-muted-foreground">{id}</span>
      </div>
      {(method || path) && (
        <div className="mb-3 flex flex-wrap items-center gap-2">
          {method ? (
            <span className="rounded bg-green-50 px-2 py-0.5 font-mono text-xs font-semibold text-green-700 dark:bg-green-950/60 dark:text-green-300">
              {method}
            </span>
          ) : null}
          {path ? (
            <span className="font-mono text-xs text-foreground">{path}</span>
          ) : null}
        </div>
      )}
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
