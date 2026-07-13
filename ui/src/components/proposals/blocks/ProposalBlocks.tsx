import { useMemo } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";

import { getBlockByTag } from "@/lib/blockRegistry";

import { BlockDepthContext, MAX_BLOCK_DEPTH, useBlockDepth } from "./blockDepth";
import { parseMdxBody } from "./parseMdxBody";

/** Wide GFM tables (e.g. a multi-column criteria catalog) overflow the proposal
 *  column and break the layout. Wrap them so they scroll horizontally instead. */
// eslint-disable-next-line react-refresh/only-export-components -- shared ReactMarkdown component map, consumed by ProposalsPage and re-exported via blocks/index.ts.
export const proposalMarkdownComponents: Components = {
  table({ node: _node, ...props }) {
    return (
      <div className="overflow-x-auto">
        <table {...props} />
      </div>
    );
  },
};

/**
 * Render an MDX body as plain GitHub-flavoured markdown — the recursion-cap
 * fallback for container children that have nested too deep, and the render
 * path for ordinary markdown segments.
 */
function MarkdownBody({ text }: { text: string }) {
  return (
    <div className="prose prose-sm max-w-none dark:prose-invert">
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={proposalMarkdownComponents}>
        {text}
      </ReactMarkdown>
    </div>
  );
}

export interface ProposalBlocksProps {
  /** A proposal-MDX body string (top-level or a container child). */
  body: string;
}

/**
 * Parse a proposal-MDX `body` and render each segment: custom block tags are
 * resolved through the block registry and rendered as React components;
 * everything else is rendered as GitHub-flavoured markdown.
 *
 * This is the SINGLE segment→component renderer shared by the top-level
 * `BlockRenderer` and the recursive container blocks (`Tabs`, `Columns`) so
 * there is never a second, drifting renderer. Container blocks call this with
 * each child `body`; the `BlockDepthContext` increments on every nested render
 * and, beyond {@link MAX_BLOCK_DEPTH}, the body is rendered as plain markdown to
 * cap runaway nesting.
 *
 * Each block is wrapped in a `<div id={blockId}>` so browser anchors and the
 * deep-link highlight can scroll to a specific block.
 */
export function ProposalBlocks({ body }: ProposalBlocksProps) {
  const depth = useBlockDepth();
  const segments = useMemo(() => parseMdxBody(body), [body]);

  // Depth cap reached: stop parsing nested blocks and render the body as plain
  // markdown so a pathological container-in-container body cannot loop.
  if (depth > MAX_BLOCK_DEPTH) {
    return <MarkdownBody text={body} />;
  }

  return (
    <BlockDepthContext.Provider value={depth + 1}>
      <div className="space-y-4">
        {segments.map((segment) => {
          if (segment.kind === "markdown") {
            return (
              <MarkdownBody
                key={`md-${segment.text.slice(0, 32)}`}
                text={segment.text}
              />
            );
          }

          // Block segment — look up the component in the registry.
          const def = getBlockByTag(segment.tag);
          if (!def) {
            // Unknown/unregistered tag (e.g. a retired block type left in an old
            // proposal): degrade gracefully by rendering the block's CHILDREN as
            // GitHub-flavoured markdown so the authored content still shows. For
            // example a leftover `<Table>` wrapping a pipe table still renders the
            // table (remark-gfm), rather than a hard error or raw MDX source.
            return (
              <MarkdownBody
                key={`unknown-${segment.index}`}
                text={segment.content}
              />
            );
          }

          const BlockComponent = def.component;

          return (
            <div
              key={`block-${segment.id}-${segment.index}`}
              id={segment.id}
              className="scroll-mt-4"
            >
              <BlockComponent id={segment.id} attributes={segment.attributes}>
                {segment.content}
              </BlockComponent>
            </div>
          );
        })}
      </div>
    </BlockDepthContext.Provider>
  );
}
