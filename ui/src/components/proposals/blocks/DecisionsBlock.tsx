import type { BlockProps } from "./types";
import { BlockMarkdown, BlockShell } from "./BlockShell";

export function DecisionsBlock({ children }: BlockProps) {
  return (
    <BlockShell label="Decisions" accent="text-amber-500">
      <BlockMarkdown>{children}</BlockMarkdown>
    </BlockShell>
  );
}
