import { ProposalBlocks } from "./ProposalBlocks";

export interface BlockRendererProps {
  body: string;
}

/**
 * Parse an MDX proposal body and render each segment: custom block tags are
 * resolved through the block registry and rendered as React components;
 * everything else is rendered as GitHub-flavoured markdown.
 *
 * This is the top-level entry point; the actual segment→component rendering
 * lives in {@link ProposalBlocks}, the single shared renderer that recursive
 * container blocks (`Tabs`, `Columns`) reuse for their child bodies so there is
 * never a second, drifting renderer.
 *
 * Each block is wrapped in a `<div id={blockId}>` so browser anchors and the
 * deep-link highlight can scroll to a specific block. Feedback is a single
 * proposal-level thread (see `FeedbackThread`), not a per-block rail, so blocks
 * render full-width.
 */
export function BlockRenderer({ body }: BlockRendererProps) {
  return <ProposalBlocks body={body} />;
}
