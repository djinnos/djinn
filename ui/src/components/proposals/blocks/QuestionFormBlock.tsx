import { useMemo } from "react";
import { HelpCircleIcon } from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { BlockMarkdown, BlockShell } from "./BlockShell";
import { Badge } from "./blockBadges";
import type { BlockProps } from "./types";
import { parseQuestions } from "./questionForm";

/**
 * QuestionForm — a READ-ONLY list of outstanding questions on a proposal.
 *
 * This is deliberately NOT a form: there are no inputs, radios, checkboxes, or
 * submit affordances. People answer out-of-band via proposal chat. The block
 * just renders the questions the LLM hand-wrote (as the block's children prose)
 * more nicely than a plain-markdown blob.
 *
 * The freeform children are parsed (`parseQuestions`) into individual questions
 * — a numbered line, a `?`-terminated sentence, a bolded line, or a `###`
 * heading starts a new question; following lines/bullets are that question's
 * sub-detail. Each renders as a clean numbered card with a question-mark marker,
 * the markdown-rendered question text, optional muted sub-detail, and a small
 * "recommended" badge when the author tagged an item. If parsing yields nothing
 * we fall back to the original `BlockMarkdown` render so a proposal never blanks.
 */
export function QuestionFormBlock({ children }: BlockProps) {
  const content = typeof children === "string" ? children.trim() : "";
  const questions = useMemo(() => parseQuestions(content), [content]);

  // Parser produced nothing usable (or children weren't a string) — fall back
  // to the original markdown render so an existing proposal never blanks.
  if (questions.length === 0) {
    return (
      <BlockShell label="Open Questions" accent="text-pink-400">
        <BlockMarkdown>{children}</BlockMarkdown>
      </BlockShell>
    );
  }

  return (
    <BlockShell
      label="Open Questions"
      accent="text-pink-400"
      meta={
        <span>
          {questions.length}{" "}
          {questions.length === 1 ? "question" : "questions"}
        </span>
      }
    >
      <ol className="space-y-2.5">
        {questions.map((q, index) => (
          <li
            key={`${index}-${q.question.slice(0, 24)}`}
            className="flex gap-3 rounded-lg border bg-muted/20 px-3.5 py-3"
          >
            <span
              className="mt-0.5 flex size-6 shrink-0 items-center justify-center rounded-full bg-pink-500/15 text-pink-300"
              aria-hidden
            >
              <HugeiconsIcon icon={HelpCircleIcon} size={15} />
            </span>
            <div className="min-w-0 flex-1">
              <div className="flex items-start justify-between gap-2">
                <div className="prose prose-sm min-w-0 max-w-none font-medium text-foreground dark:prose-invert prose-p:my-0">
                  <BlockMarkdown>{q.question}</BlockMarkdown>
                </div>
                {q.recommended && (
                  <Badge accent="emerald" className="mt-0.5 shrink-0">
                    recommended
                  </Badge>
                )}
              </div>
              {q.detail.length > 0 && (
                <ul className="mt-1.5 space-y-1 border-l border-border/60 pl-3 text-xs text-muted-foreground">
                  {q.detail.map((line, di) => (
                    <li
                      key={di}
                      className="prose prose-sm max-w-none text-xs dark:prose-invert prose-p:my-0"
                    >
                      <BlockMarkdown>{line}</BlockMarkdown>
                    </li>
                  ))}
                </ul>
              )}
            </div>
          </li>
        ))}
      </ol>
    </BlockShell>
  );
}
