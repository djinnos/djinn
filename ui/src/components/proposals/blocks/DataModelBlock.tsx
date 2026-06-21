import type { BlockProps } from "./types";
import { BlockMarkdown, BlockShell } from "./BlockShell";

export function DataModelBlock({ children }: BlockProps) {
  return (
    <BlockShell label="Data Model" accent="text-blue-400">
      <BlockMarkdown>{children}</BlockMarkdown>
    </BlockShell>
  );
}
