import type { ReactNode } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import { cn } from "@/lib/utils";

export interface BlockShellProps {
  /** Short type label shown in the header, e.g. "Diagram". */
  label: string;
  /**
   * Tailwind text-color class for the label, giving each block type a subtle
   * accent without the heavy colored pills we used to render.
   */
  accent?: string;
  /** Optional right-aligned metadata (language, path, method, root…). */
  meta?: ReactNode;
  /** When true, the body gets no padding — for full-bleed code/diagram surfaces. */
  flush?: boolean;
  className?: string;
  children: ReactNode;
}

/**
 * Shared chrome for structured proposal blocks: a clean card with a quiet,
 * single-line header (uppercase label + optional accent + optional meta) and a
 * body. Replaces the old per-block "big colored pill + mono block id" header so
 * the spec reads like an intentional document instead of a wall of badges.
 *
 * The block's anchor `id` lives on the wrapper `<div>` in `BlockRenderer`, so
 * the shell deliberately does not render one (avoids duplicate DOM ids).
 */
export function BlockShell({
  label,
  accent,
  meta,
  flush,
  className,
  children,
}: BlockShellProps) {
  return (
    <div className={cn("overflow-hidden rounded-lg border bg-card", className)}>
      <div className="flex items-center justify-between gap-3 border-b bg-muted/30 px-4 py-2">
        <span
          className={cn(
            "text-[11px] font-semibold uppercase tracking-wider",
            accent ?? "text-muted-foreground",
          )}
        >
          {label}
        </span>
        {meta ? (
          <div className="flex min-w-0 items-center gap-2 text-xs text-muted-foreground">
            {meta}
          </div>
        ) : null}
      </div>
      <div className={flush ? "" : "p-4"}>{children}</div>
    </div>
  );
}

/**
 * Render a block's children as GitHub-flavoured markdown when it's a string,
 * or pass through pre-rendered nodes. Keeps the `prose` styling identical
 * across every text-bearing block.
 */
export function BlockMarkdown({ children }: { children?: ReactNode }) {
  return (
    <div className="prose prose-sm max-w-none dark:prose-invert">
      {typeof children === "string" ? (
        <ReactMarkdown remarkPlugins={[remarkGfm]}>{children}</ReactMarkdown>
      ) : (
        children
      )}
    </div>
  );
}
