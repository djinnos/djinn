import type { BlockProps } from "./types";
import { BlockMarkdown } from "./BlockShell";
import { unwrapExprValue } from "./exprAttr";

/**
 * RichText — plain document prose. This is the proposal's connective tissue, so
 * it renders with no card or type label (a "Rich Text" badge over a real "##"
 * heading is just noise); the markdown headings carry the structure.
 *
 * The prose comes from the `content` schema field (the registry's authored
 * form, e.g. `<RichText content="…" />`) or the block children — reading
 * children only dropped attribute-authored prose entirely (same class of bug
 * as Diagram `source` / AnnotatedCode `code`).
 */
export function RichText({ attributes, children }: BlockProps) {
  const attr = attributes.content;
  const content =
    typeof attr === "string" && attr.trim().length > 0
      ? unwrapExprValue(attr)
      : children;
  return <BlockMarkdown>{content}</BlockMarkdown>;
}
