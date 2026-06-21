import type { BlockProps } from "./types";
import { BlockMarkdown, BlockShell } from "./BlockShell";

export function QuestionFormBlock({ children }: BlockProps) {
  return (
    <BlockShell label="Open Questions" accent="text-pink-400">
      <BlockMarkdown>{children}</BlockMarkdown>
    </BlockShell>
  );
}
