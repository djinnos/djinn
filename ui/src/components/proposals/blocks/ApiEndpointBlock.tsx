import type { BlockProps } from "./types";
import { BlockMarkdown, BlockShell } from "./BlockShell";

export function ApiEndpointBlock({ attributes, children }: BlockProps) {
  const method = attributes.method?.toUpperCase();
  const path = attributes.path;

  return (
    <BlockShell
      label="API Endpoint"
      accent="text-green-400"
      meta={
        method || path ? (
          <span className="flex min-w-0 items-center gap-2 font-mono">
            {method ? (
              <span className="font-semibold text-green-400">{method}</span>
            ) : null}
            {path ? <span className="truncate text-foreground">{path}</span> : null}
          </span>
        ) : null
      }
    >
      <BlockMarkdown>{children}</BlockMarkdown>
    </BlockShell>
  );
}
