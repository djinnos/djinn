import type { BlockProps } from "./types";

/**
 * Stub component for the `file-tree` block type.
 *
 * The full rendering (tree structure, root path display) will be implemented by
 * the sibling task that owns the P2 block React components. This minimal stub
 * satisfies the registry/barrel contract so downstream tasks can compile.
 */
export function FileTreeBlock({ id, children }: BlockProps) {
  return (
    <div id={id} className="rounded-lg border bg-card p-4 shadow-sm">
      <span className="mb-2 block text-xs font-medium text-muted-foreground">
        file-tree
      </span>
      {children}
    </div>
  );
}
