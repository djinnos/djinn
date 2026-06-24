import { useMemo } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  Alert02Icon,
  CheckmarkCircle02Icon,
  Shield01Icon,
} from "@hugeicons/core-free-icons";
import { Badge } from "@/components/ui/badge";
import { Label } from "@/components/ui/label";
import { cn } from "@/lib/utils";
import { relativeTime } from "@/components/memory/memoryUtils";
import type { ProposalDebateTrailRow } from "@/api/types";

/** Derived display state for a single debate-trail row. */
function entryState(
  row: ProposalDebateTrailRow,
): "open" | "resolved" | "reopened" {
  if (row.resolved_at && row.reopened_at) return "reopened";
  if (row.resolved_at) return "resolved";
  return "open";
}

/** Human-readable label for the entry state. */
function stateLabel(state: "open" | "resolved" | "reopened"): string {
  switch (state) {
    case "open":
      return "Open";
    case "resolved":
      return "Resolved";
    case "reopened":
      return "Reopened";
  }
}

/** Badge variant for entry state. */
function stateBadgeVariant(
  state: "open" | "resolved" | "reopened",
): "default" | "secondary" | "outline" {
  switch (state) {
    case "open":
      return "default";
    case "resolved":
      return "secondary";
    case "reopened":
      return "outline";
  }
}

/** Icon for the debate row kind. */
function KindIcon({ kind }: { kind: string }) {
  switch (kind) {
    case "objection":
      return <HugeiconsIcon icon={Alert02Icon} size={14} className="text-destructive" />;
    case "rebuttal":
      return <HugeiconsIcon icon={Shield01Icon} size={14} className="text-blue-500" />;
    case "verdict":
      return <HugeiconsIcon icon={CheckmarkCircle02Icon} size={14} className="text-green-600" />;
    default:
      return null;
  }
}

/** Badge for the debate row kind. */
function KindBadge({ kind }: { kind: string }) {
  const variantMap: Record<string, "default" | "secondary" | "destructive" | "outline"> = {
    objection: "destructive",
    rebuttal: "secondary",
    verdict: "default",
  };
  return (
    <Badge variant={variantMap[kind] ?? "outline"} className="font-mono capitalize">
      <KindIcon kind={kind} />
      <span className="ml-1">{kind}</span>
    </Badge>
  );
}

/** Badge for blocking vs non-blocking. */
function BlockingBadge({ blocking }: { blocking: boolean }) {
  return (
    <Badge
      variant={blocking ? "destructive" : "outline"}
      className={cn("text-xs", !blocking && "text-muted-foreground")}
    >
      {blocking ? "Blocking" : "Non-blocking"}
    </Badge>
  );
}

/** Source/author attribution line. */
function AuthorLine({ row }: { row: ProposalDebateTrailRow }) {
  const authorName =
    row.author_kind === "agent"
      ? row.author_model ?? "agent"
      : row.author_user_id ?? "user";

  return (
    <span className="flex items-center gap-1.5 text-xs text-muted-foreground">
      {row.agent_role && (
        <Badge variant="outline" className="text-[10px] capitalize">
          {row.agent_role}
        </Badge>
      )}
      <span className="capitalize">{row.author_kind}</span>
      <span>·</span>
      <span>{authorName}</span>
    </span>
  );
}

/** Grouped debate-trail rows by round. */
interface RoundGroup {
  round: number;
  rows: ProposalDebateTrailRow[];
}

/**
 * Read-only debate-trail viewer for proposals.
 *
 * Groups rows by `round`, displays kind (objection/rebuttal/verdict),
 * blocking/non-blocking badges, open/resolved/reopened state, role/speaker
 * labels, revision anchor, timestamps, and markdown body content.
 *
 * This component is intentionally read-only. Resolve/reopen actions are
 * handled by the follow-up task (6dav).
 */
export function DebateTrail({
  debateTrail,
  className,
}: {
  debateTrail: ProposalDebateTrailRow[];
  className?: string;
}) {
  const groups = useMemo<RoundGroup[]>(() => {
    if (debateTrail.length === 0) return [];

    const byRound = new Map<number, ProposalDebateTrailRow[]>();
    for (const row of debateTrail) {
      const existing = byRound.get(row.round);
      if (existing) {
        existing.push(row);
      } else {
        byRound.set(row.round, [row]);
      }
    }

    return [...byRound.entries()]
      .sort(([a], [b]) => a - b)
      .map(([round, rows]) => ({
        round,
        rows: rows.sort(
          (x, y) => x.created_at.localeCompare(y.created_at) || x.id.localeCompare(y.id),
        ),
      }));
  }, [debateTrail]);

  if (groups.length === 0) return null;

  return (
    <div className={cn("space-y-2", className)}>
      <Label className="text-xs uppercase text-muted-foreground">
        Debate trail
      </Label>
      <div className="space-y-4">
        {groups.map((group) => (
          <RoundSection key={group.round} group={group} />
        ))}
      </div>
    </div>
  );
}

function RoundSection({ group }: { group: RoundGroup }) {
  return (
    <div className="space-y-2">
      <div className="flex items-center gap-2 text-xs font-medium text-muted-foreground">
        <Badge variant="outline" className="font-mono">
          Round {group.round}
        </Badge>
        <span className="text-muted-foreground/60">
          {group.rows.length} {group.rows.length === 1 ? "entry" : "entries"}
        </span>
      </div>
      <ul className="divide-y rounded-md border">
        {group.rows.map((row) => (
          <DebateEntry key={row.id} row={row} />
        ))}
      </ul>
    </div>
  );
}

function DebateEntry({ row }: { row: ProposalDebateTrailRow }) {
  const state = entryState(row);

  return (
    <li className="space-y-2 px-3 py-3">
      {/* Header row: kind badge, blocking badge, state badge, timestamp */}
      <div className="flex flex-wrap items-center gap-2">
        <KindBadge kind={row.kind} />
        <BlockingBadge blocking={row.blocking} />
        <Badge
          variant={stateBadgeVariant(state)}
          className={cn(
            "text-xs",
            state === "reopened" && "border-amber-500 text-amber-700 dark:text-amber-300",
          )}
        >
          {stateLabel(state)}
        </Badge>
        <Badge variant="secondary" className="font-mono text-xs">
          rev {row.against_revision_seq}
        </Badge>
        <AuthorLine row={row} />
        <time
          dateTime={row.created_at}
          title={row.created_at}
          className="ml-auto shrink-0 text-xs text-muted-foreground"
        >
          {relativeTime(row.created_at)}
        </time>
      </div>

      {/* Body content */}
      <div className="prose prose-sm max-w-none dark:prose-invert">
        <ReactMarkdown remarkPlugins={[remarkGfm]}>
          {row.body || "_Empty entry._"}
        </ReactMarkdown>
      </div>
    </li>
  );
}
