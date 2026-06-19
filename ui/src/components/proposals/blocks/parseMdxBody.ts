import type { ProposalFeedback } from "@/api/types";

// Matches <tag-name attributes>content</tag-name> — same pattern as the
// server-side Rust parser so block boundaries are identical in both places.
const BLOCK_REGEX = /<([a-z-]+)([^>]*)>([\s\S]*?)<\/\1>/g;

/** Key="value" pairs inside a block's opening tag. */
const ATTR_REGEX = /([a-z_-]+)="([^"]*)"/g;

// ──────────────────────────────────────────────────────────────────────────────
// Types
// ──────────────────────────────────────────────────────────────────────────────

export interface BlockRendererProps {
  body: string;
  feedback?: ProposalFeedback[];
}

interface BlockSegment {
  kind: "block";
  tag: string;
  attributes: Record<string, string>;
  content: string;
  /** The `id` attribute value, or an auto-generated fallback. */
  id: string;
  /** Index of the block within the body (for stable fallback ids). */
  index: number;
}

interface MarkdownSegment {
  kind: "markdown";
  text: string;
}

type BodySegment = BlockSegment | MarkdownSegment;

// ──────────────────────────────────────────────────────────────────────────────
// Parsing
// ──────────────────────────────────────────────────────────────────────────────

function parseAttributes(attrString: string): Record<string, string> {
  const attrs: Record<string, string> = {};
  let m: RegExpExecArray | null;
  ATTR_REGEX.lastIndex = 0;
  while ((m = ATTR_REGEX.exec(attrString)) !== null) {
    attrs[m[1]] = m[2];
  }
  return attrs;
}

/**
 * Split an MDX proposal body into alternating segments of regular markdown
 * and block content. The approach mirrors the server-side Rust parser: a
 * single regex pass captures each custom-tag block; the text between matches
 * is collected as markdown segments.
 */
export function parseMdxBody(body: string): BodySegment[] {
  const segments: BodySegment[] = [];
  let lastIndex = 0;
  let blockIndex = 0;

  // Reset global regex state
  BLOCK_REGEX.lastIndex = 0;

  let match: RegExpExecArray | null;
  while ((match = BLOCK_REGEX.exec(body)) !== null) {
    // Text before this match is plain markdown
    if (match.index > lastIndex) {
      const mdText = body.slice(lastIndex, match.index);
      if (mdText.trim().length > 0) {
        segments.push({ kind: "markdown", text: mdText });
      }
    }

    const tag = match[1];
    const attrString = match[2] ?? "";
    const content = match[3] ?? "";
    const attributes = parseAttributes(attrString);
    const id = attributes["id"] ?? `block-${blockIndex}`;

    segments.push({
      kind: "block",
      tag,
      attributes,
      content,
      id,
      index: blockIndex,
    });

    blockIndex++;
    lastIndex = match.index + match[0].length;
  }

  // Trailing markdown after the last block
  if (lastIndex < body.length) {
    const mdText = body.slice(lastIndex);
    if (mdText.trim().length > 0) {
      segments.push({ kind: "markdown", text: mdText });
    }
  }

  return segments;
}
