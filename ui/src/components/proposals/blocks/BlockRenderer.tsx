import { useMemo } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import { getBlockByTag } from "@/lib/blockRegistry";

import { parseMdxBody } from "./parseMdxBody";

export interface BlockRendererProps {
  body: string;
}

/**
 * Parse an MDX proposal body and render each segment: custom block tags are
 * resolved through the block registry and rendered as React components;
 * everything else is rendered as GitHub-flavoured markdown.
 *
 * Each block is wrapped in a `<div id={blockId}>` so browser anchors and the
 * deep-link highlight can scroll to a specific block. Feedback is a single
 * proposal-level thread (see `FeedbackThread`), not a per-block rail, so blocks
 * render full-width.
 */
export function BlockRenderer({ body }: BlockRendererProps) {
  const segments = useMemo(() => parseMdxBody(body), [body]);

  return (
    <div className="space-y-4">
      {segments.map((segment) => {
        if (segment.kind === "markdown") {
          return (
            <div
              key={`md-${segment.text.slice(0, 32)}`}
              className="prose prose-sm max-w-none dark:prose-invert"
            >
              <ReactMarkdown remarkPlugins={[remarkGfm]}>
                {segment.text}
              </ReactMarkdown>
            </div>
          );
        }

        // Block segment — look up the component in the registry
        const def = getBlockByTag(segment.tag);
        if (!def) {
          // Unknown tag: render as code-fenced text so nothing is silently lost
          return (
            <pre
              key={`unknown-${segment.index}`}
              className="overflow-x-auto rounded border border-dashed border-amber-300 bg-amber-50 p-3 text-xs text-amber-900 dark:border-amber-700 dark:bg-amber-950 dark:text-amber-100"
            >
              {`<${segment.tag} ...>${segment.content}</${segment.tag}>`}
            </pre>
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
  );
}
