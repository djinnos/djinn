import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import type { BlockProps } from "./types";

export function FileTreeBlock({ id, attributes, children }: BlockProps) {
  const root = attributes.root;

  return (
    <div id={id} className="rounded-lg border bg-card p-4 shadow-sm">
      <div className="mb-2 flex items-center justify-between gap-3">
        <span className="rounded-full bg-slate-100 px-2.5 py-0.5 text-xs font-semibold text-slate-800 dark:bg-slate-900 dark:text-slate-200">
          File Tree
        </span>
        <span className="font-mono text-xs text-muted-foreground">{id}</span>
      </div>
      {root ? (
        <div className="mb-3 font-mono text-xs text-muted-foreground">
          {root}
        </div>
      ) : null}
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
