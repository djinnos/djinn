import type { BlockProps } from "./types";

/**
 * Stub component for the `api-endpoint` block type.
 *
 * The full rendering (method badge, path, request/response schemas) will be
 * implemented by the sibling task that owns the P2 block React components.
 * This minimal stub satisfies the registry/barrel contract so downstream
 * tasks can compile.
 */
export function ApiEndpointBlock({ id, children }: BlockProps) {
  return (
    <div id={id} className="rounded-lg border bg-card p-4 shadow-sm">
      <span className="mb-2 block text-xs font-medium text-muted-foreground">
        api-endpoint
      </span>
      {children}
    </div>
  );
}
