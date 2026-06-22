import type { ReactNode } from "react";
import { HugeiconsIcon } from "@hugeicons/react";

import { BlockMarkdown } from "./BlockShell";

export interface ContainerFallbackProps {
  /** Which container failed to parse — drives the muted notice copy. */
  kind: "tabs" | "columns";
  /** The raw `{...}` attribute text, shown for authoring debugging. */
  rawAttribute?: string;
  /** Leading icon (a hugeicons named import). */
  icon: Parameters<typeof HugeiconsIcon>[0]["icon"];
  /** The block's own children markdown, if any, preferred over the notice. */
  children?: ReactNode;
}

/**
 * Calm fallback for a container block (`Tabs` / `Columns`) whose JSON attribute
 * is missing or invalid. Prefers rendering the block's own children markdown
 * (so authored content is never lost); otherwise shows a muted notice plus the
 * raw attribute text. Never throws, never blanks.
 */
export function ContainerFallback({
  kind,
  rawAttribute,
  icon,
  children,
}: ContainerFallbackProps) {
  const body = typeof children === "string" ? children.trim() : "";
  if (body.length > 0) {
    return (
      <div className="rounded-lg border bg-card p-4">
        <BlockMarkdown>{body}</BlockMarkdown>
      </div>
    );
  }

  const label = kind === "tabs" ? "Could not render tabs" : "Could not render columns";
  return (
    <div className="rounded-lg border border-dashed bg-muted/20 p-4 text-sm text-muted-foreground">
      <div className="flex items-center gap-2">
        <HugeiconsIcon icon={icon} size={15} className="shrink-0" aria-hidden />
        <span>{label}</span>
      </div>
      {rawAttribute ? (
        <pre className="mt-2 overflow-x-auto overflow-y-hidden rounded bg-background/60 p-2 text-xs">
          {rawAttribute}
        </pre>
      ) : null}
    </div>
  );
}
