import { useMemo } from "react";

import { BlockMarkdown, BlockShell } from "./BlockShell";
import { Badge } from "./blockBadges";
import type { AccentColor } from "./blockBadgeColors";
import type { BlockProps } from "./types";
import {
  parseDecisions,
  type DecisionStatus,
  type ParsedDecision,
} from "./decisions";

/**
 * Decisions — an ADR-style list of architecture decision records.
 *
 * djinn does not serialize a structured `items={…}` JSON prop; the decisions are
 * the block's *children markdown* (one `### Title` heading per decision, an
 * optional `Status: <token>` line, then optional Context/Decision/Consequences
 * sub-sections or freeform body). We parse that into cards (`parseDecisions`) and
 * render each as a flattened row — title + DECLARED status badge, then sections.
 *
 * Status is read ONLY from an explicit declared token (the ADR vocabulary:
 * proposed · accepted · rejected · deprecated · superseded). It is never inferred
 * from English prose; a card with no declared status shows NO badge.
 *
 * If parsing yields no recognisable decisions, we fall back to the original
 * `BlockMarkdown` render so a hand-authored block never blanks or crashes.
 */
export function DecisionsBlock({ children }: BlockProps) {
  const content = typeof children === "string" ? children : "";
  const decisions = useMemo(() => parseDecisions(content), [content]);

  // Parsing produced nothing usable — fall back to the raw markdown render so a
  // block that doesn't match our ADR shape is never lost.
  if (decisions.length === 0) {
    return (
      <BlockShell label="Decisions" accent="text-amber-500">
        <BlockMarkdown>{children}</BlockMarkdown>
      </BlockShell>
    );
  }

  return (
    <BlockShell
      label="Decisions"
      accent="text-amber-500"
      meta={
        <span>
          {decisions.length} {decisions.length === 1 ? "decision" : "decisions"}
        </span>
      }
    >
      <ol className="divide-y divide-border/60">
        {decisions.map((decision, index) => (
          <DecisionCard
            key={`${index}-${decision.title.slice(0, 24)}`}
            decision={decision}
          />
        ))}
      </ol>
    </BlockShell>
  );
}

/* ── Status → color ─────────────────────────────────────────────────────────── */

/**
 * ADR status → accent color. Tones chosen for intuitive read: proposed (in-flight)
 * slate, accepted (good) emerald, rejected (no) red, deprecated (fading) amber,
 * superseded (replaced) violet.
 */
const STATUS_ACCENT: Record<DecisionStatus, AccentColor> = {
  proposed: "slate",
  accepted: "emerald",
  rejected: "red",
  deprecated: "amber",
  superseded: "violet",
};

/* ── Decision card ──────────────────────────────────────────────────────────── */

function DecisionCard({ decision }: { decision: ParsedDecision }) {
  const { title, status, supersededBy, sections, body } = decision;

  return (
    <li className="py-3 first:pt-0 last:pb-0">
      <div className="flex items-start justify-between gap-2">
        <h4 className="min-w-0 flex-1 text-sm font-semibold text-foreground">
          {title}
        </h4>
        {status && (
          <Badge accent={STATUS_ACCENT[status]} className="mt-0.5 shrink-0">
            {status}
          </Badge>
        )}
      </div>

      {supersededBy && (
        <p className="mt-0.5 text-xs text-muted-foreground">
          superseded by{" "}
          <span className="font-medium text-foreground/80">{supersededBy}</span>
        </p>
      )}

      {sections ? (
        <div className="mt-2 space-y-2">
          {sections.context && (
            <DecisionSection label="Context" markdown={sections.context} />
          )}
          {sections.decision && (
            <DecisionSection label="Decision" markdown={sections.decision} />
          )}
          {sections.consequences && (
            <DecisionSection
              label="Consequences"
              markdown={sections.consequences}
            />
          )}
        </div>
      ) : null}

      {body && (
        <div className="prose prose-sm mt-2 max-w-none text-sm dark:prose-invert prose-p:my-1">
          <BlockMarkdown>{body}</BlockMarkdown>
        </div>
      )}
    </li>
  );
}

/* ── ADR sub-section ────────────────────────────────────────────────────────── */

function DecisionSection({
  label,
  markdown,
}: {
  label: string;
  markdown: string;
}) {
  return (
    <div className="border-l border-border/60 pl-3">
      <div className="text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
        {label}
      </div>
      <div className="prose prose-sm mt-0.5 max-w-none text-sm dark:prose-invert prose-p:my-1">
        <BlockMarkdown>{markdown}</BlockMarkdown>
      </div>
    </div>
  );
}
