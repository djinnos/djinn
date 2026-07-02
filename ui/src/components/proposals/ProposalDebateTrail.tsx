import { useMemo, useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  Alert02Icon,
  ArrowDown01Icon,
  CancelCircleIcon,
  CheckmarkCircle02Icon,
  Comment01Icon,
  JusticeScale01Icon,
} from "@hugeicons/core-free-icons";
import { Badge } from "@/components/ui/badge";
import { relativeTime } from "@/components/memory/memoryUtils";
import { cn } from "@/lib/utils";
import type { ProposalDebateTrailRow } from "@/api/types";
import { BlockMarkdown } from "./blocks/BlockShell";
import { verdictOutcome } from "./debateTrailUtils";

/** Icon for a debate entry by kind. */
function entryIcon(row: ProposalDebateTrailRow) {
  if (row.kind === "verdict") return JusticeScale01Icon;
  if (row.blocking) return Alert02Icon;
  return Comment01Icon;
}

interface RoundGroup {
  round: number;
  entries: ProposalDebateTrailRow[];
  objections: number;
  resolvedObjections: number;
  verdict: ProposalDebateTrailRow | null;
  fromRev: number;
  toRev: number;
}

function groupByRound(trail: ProposalDebateTrailRow[]): RoundGroup[] {
  const byRound = new Map<number, ProposalDebateTrailRow[]>();
  for (const row of trail) {
    const list = byRound.get(row.round) ?? [];
    list.push(row);
    byRound.set(row.round, list);
  }

  return [...byRound.entries()]
    .map(([round, rows]) => {
      const entries = [...rows].sort(
        (a, b) =>
          a.created_at.localeCompare(b.created_at) || a.id.localeCompare(b.id),
      );
      const objections = entries.filter((e) => e.kind === "objection");
      const verdicts = entries.filter((e) => e.kind === "verdict");
      const revs = entries.map((e) => e.against_revision_seq);
      return {
        round,
        entries,
        objections: objections.length,
        resolvedObjections: objections.filter((e) => e.resolved_at != null)
          .length,
        verdict: verdicts.length ? verdicts[verdicts.length - 1] : null,
        fromRev: Math.min(...revs),
        toRev: Math.max(...revs),
      };
    })
    .sort((a, b) => a.round - b.round);
}

function EntryRow({ row }: { row: ProposalDebateTrailRow }) {
  const [open, setOpen] = useState(false);
  const outcome = row.kind === "verdict" ? verdictOutcome(row) : null;
  const excerpt = row.body.replace(/\s+/g, " ").trim();
  const isLong = excerpt.length > 160 || row.body.trim().includes("\n");

  return (
    <li className="rounded-md border bg-background/60 px-2.5 py-2">
      <div className="flex flex-wrap items-center gap-2 text-xs">
        <HugeiconsIcon
          icon={entryIcon(row)}
          size={13}
          className={cn(
            "shrink-0",
            row.blocking ? "text-destructive" : "text-muted-foreground",
          )}
        />
        <span className="font-medium uppercase tracking-wide text-foreground">
          {row.agent_role || row.kind}
        </span>
        {outcome && (
          <span
            className={cn(
              "inline-flex items-center gap-1",
              outcome.positive
                ? "text-green-600 dark:text-green-400"
                : "text-destructive",
            )}
          >
            <HugeiconsIcon
              icon={outcome.positive ? CheckmarkCircle02Icon : CancelCircleIcon}
              size={12}
            />
            verdict: {outcome.label}
          </span>
        )}
        {row.blocking && (
          <Badge variant="destructive" className="text-[10px]">
            blocking
          </Badge>
        )}
        {row.resolved_at && (
          <span className="inline-flex items-center gap-1 text-green-600 dark:text-green-400">
            <HugeiconsIcon icon={CheckmarkCircle02Icon} size={12} />
            resolved {relativeTime(row.resolved_at)}
          </span>
        )}
        {row.reopened_at && (
          <Badge variant="outline" className="text-[10px]">
            reopened {relativeTime(row.reopened_at)}
          </Badge>
        )}
        <span className="ml-auto shrink-0 text-muted-foreground">
          {relativeTime(row.created_at)}
        </span>
      </div>

      {open ? (
        <div className="mt-1.5">
          <BlockMarkdown>{row.body}</BlockMarkdown>
        </div>
      ) : (
        <p className="mt-1 line-clamp-2 text-xs text-muted-foreground">
          {excerpt || "(no content)"}
        </p>
      )}
      {isLong && (
        <button
          type="button"
          aria-expanded={open}
          onClick={() => setOpen((v) => !v)}
          className="mt-1 text-[11px] text-muted-foreground underline-offset-2 hover:text-foreground hover:underline"
        >
          {open ? "Collapse" : "Expand"}
        </button>
      )}
    </li>
  );
}

function RoundBlock({
  group,
  defaultOpen,
}: {
  group: RoundGroup;
  defaultOpen: boolean;
}) {
  const [open, setOpen] = useState(defaultOpen);
  const revLabel =
    group.fromRev === group.toRev
      ? `rev ${group.fromRev}`
      : `rev ${group.fromRev}→${group.toRev}`;
  const outcome = group.verdict ? verdictOutcome(group.verdict) : null;

  return (
    <li>
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
        className="flex w-full flex-wrap items-center gap-2 px-3 py-2 text-left text-sm hover:bg-muted/40 focus:outline-none focus:ring-2 focus:ring-ring"
      >
        <HugeiconsIcon
          icon={ArrowDown01Icon}
          size={14}
          className={cn(
            "shrink-0 text-muted-foreground transition-transform",
            open && "rotate-180",
          )}
        />
        <span className="font-medium">Round {group.round}</span>
        <span className="font-mono text-xs text-muted-foreground">
          vs {revLabel}
        </span>
        <span className="text-xs text-muted-foreground">
          {group.objections} {group.objections === 1 ? "objection" : "objections"}
          {group.objections > 0 &&
            ` (${group.resolvedObjections} ✓ resolved)`}
        </span>
        {outcome && (
          <span
            className={cn(
              "ml-auto inline-flex items-center gap-1 text-xs",
              outcome.positive
                ? "text-green-600 dark:text-green-400"
                : "text-destructive",
            )}
          >
            <HugeiconsIcon
              icon={outcome.positive ? CheckmarkCircle02Icon : CancelCircleIcon}
              size={13}
            />
            verdict: {outcome.label}
          </span>
        )}
      </button>
      {open && (
        <ul className="space-y-1.5 px-3 pb-3">
          {group.entries.map((e) => (
            <EntryRow key={e.id} row={e} />
          ))}
        </ul>
      )}
    </li>
  );
}

/**
 * Round-by-round debate timeline for a proposal's tribunal. Groups the raw
 * `debate_trail` by round (objections, rebuttals, verdicts), summarising each
 * round on one line and expanding to the individual entries. Rounds are
 * collapsed by default except the latest, which starts open.
 */
export function ProposalDebateTrail({
  trail,
}: {
  trail: ProposalDebateTrailRow[];
}) {
  const groups = useMemo(() => groupByRound(trail), [trail]);

  if (groups.length === 0) {
    return (
      <p className="text-sm text-muted-foreground">
        No debate entries yet.
      </p>
    );
  }

  const latestRound = groups[groups.length - 1].round;

  return (
    <ul className="divide-y rounded-md border">
      {groups.map((group) => (
        <RoundBlock
          key={group.round}
          group={group}
          defaultOpen={group.round === latestRound}
        />
      ))}
    </ul>
  );
}
