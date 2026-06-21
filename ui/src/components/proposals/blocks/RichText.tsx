import type { BlockProps } from "./types";
import { BlockMarkdown } from "./BlockShell";

/**
 * RichText — plain document prose. This is the proposal's connective tissue, so
 * it renders with no card or type label (a "Rich Text" badge over a real "##"
 * heading is just noise); the markdown headings carry the structure.
 */
export function RichText({ children }: BlockProps) {
  return <BlockMarkdown>{children}</BlockMarkdown>;
}
