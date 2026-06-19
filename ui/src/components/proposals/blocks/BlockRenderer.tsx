import { useMemo } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import type { ProposalFeedback } from "@/api/types";
import { getBlockByTag } from "@/lib/blockRegistry";

import type { BlockRendererProps } from "./parseMdxBody";
import { parseMdxBody } from "./parseMdxBody";

/**
 * Parse an MDX proposal body and render each segment: custom block tags are
 * resolved through the block registry and rendered as React components;
 * everything else is rendered as GitHub-flavoured markdown.
 *
 * Each block component receives a wrapper `<div id={blockId}>` so that
 * browser anchors and feedback `target_section` references can scroll to
 * the correct location within the proposal.
 */
export function BlockRenderer({ body, feedback }: BlockRendererProps) {
  const segments = useMemo(() => parseMdxBody(body), [body]);

  // Index feedback by target_section for O(1) lookup per block
  const feedbackBySection = useMemo(() => {
    if (!feedback?.length) return new Map<string, ProposalFeedback[]>();
    const map = new Map<string, ProposalFeedback[]>();
    for (const entry of feedback) {
      if (entry.target_section) {
        const list = map.get(entry.target_section);
        if (list) {
          list.push(entry);
        } else {
          map.set(entry.target_section, [entry]);
        }
      }
    }
    return map;
  }, [feedback]);

  return (
    <>
      {segments.map((segment) => {
        if (segment.kind === "markdown") {
          return (
            <div key={`md-${segment.text.slice(0, 32)}`} className="prose prose-sm max-w-none dark:prose-invert">
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
              className="my-2 rounded border border-dashed border-amber-300 bg-amber-50 p-3 text-xs text-amber-900 dark:border-amber-700 dark:bg-amber-950 dark:text-amber-100"
            >
              {`<${segment.tag} ...>${segment.content}</${segment.tag}>`}
            </pre>
          );
        }

        const BlockComponent = def.component;
        const blockFeedback = feedbackBySection.get(segment.id);

        return (
          <div key={`block-${segment.id}-${segment.index}`} id={segment.id}>
            <BlockComponent
              id={segment.id}
              attributes={segment.attributes}
              feedback={blockFeedback}
            >
              {segment.content}
            </BlockComponent>
          </div>
        );
      })}
    </>
  );
}
