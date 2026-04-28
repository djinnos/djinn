/**
 * CitationText — markdown renderer that splits its input around
 * `[[file:...]]` and `[[symbol:...]]` tokens, rendering the text
 * pieces through Streamdown (preserving GFM, code blocks, etc.) and
 * the citations through `<CitationLink>`.
 *
 * The split is performed eagerly per-render because the chat path
 * already memoizes message content — any future perf concerns can be
 * addressed by lifting the segment list into the chatStore.
 *
 * When the message has no citations the component falls back to a
 * single Streamdown call, so behaviour is unchanged for the existing
 * non-citation chat traffic.
 */

import { useMemo } from "react";
import { Streamdown } from "streamdown";
import { Fragment, type ReactNode } from "react";

import { segmentByCitations } from "@/lib/citationParser";
import { CitationLink } from "./CitationLink";

interface CitationTextProps {
  /** Raw markdown body emitted by the model. */
  text: string;
  /** Forwarded to every Streamdown sub-render (matches existing chat styling). */
  className?: string;
}

export function CitationText({ text, className }: CitationTextProps) {
  const segments = useMemo(() => segmentByCitations(text), [text]);

  // Fast path — no citations means we can preserve the original
  // single-Streamdown shape (avoids the inline-rendering caveat
  // below for messages that don't need it).
  if (segments.length <= 1) {
    return <Streamdown className={className}>{text}</Streamdown>;
  }

  // Citations show up *inside* prose, so we render each text segment
  // through its own Streamdown. Streamdown wraps in a block element
  // by default; with multiple segments around inline citations the
  // result reads as a sequence of paragraphs separated by chips. The
  // classes below collapse adjacent paragraph margins so the prose
  // still flows tightly.
  //
  // Trade-off: a citation that lands inside a fenced code block, list
  // item, or table cell will visually break the surrounding markdown
  // (because we split at the inline token level). The model is
  // explicitly instructed in chat.md to place citations in prose, so
  // this case is rare; if it bites we'll switch to a remark plugin
  // and an mdast custom node.
  return (
    <div className={className}>
      {segments.map((seg, idx) => (
        <Fragment key={idx}>{renderSegment(seg, idx)}</Fragment>
      ))}
    </div>
  );
}

function renderSegment(
  seg: ReturnType<typeof segmentByCitations>[number],
  idx: number,
): ReactNode {
  if (seg.type === "citation") {
    return <CitationLink citation={seg.citation} />;
  }
  // Streamdown wraps in <p>; we squash margins via tailwind so the
  // citation chip flows inline with the surrounding prose.
  return (
    <Streamdown
      key={`text-${idx}`}
      className="prose prose-sm max-w-none break-words dark:prose-invert [&>:first-child]:mt-0 [&>:last-child]:mb-0 [&>p]:inline"
    >
      {seg.text}
    </Streamdown>
  );
}
