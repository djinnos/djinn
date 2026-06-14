import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { HugeiconsIcon } from "@hugeicons/react";
import { ArrowDown01Icon } from "@hugeicons/core-free-icons";
import { usersQueryOptions } from "@/api/queryOptions";
import { userDisplayName, type OrgUser } from "@/api/users";
import { UserAvatar } from "@/components/UserAvatar";
import { DiffView } from "@/components/proposals/DiffView";
import { relativeTime } from "@/components/memory/memoryUtils";
import { Badge } from "@/components/ui/badge";
import { Label } from "@/components/ui/label";
import { cn } from "@/lib/utils";
import type { ProposalDetail } from "@/lib/proposalQueries";

/**
 * Revision history for a proposal's spec. Every material edit (title/body/AC)
 * appends a full snapshot to `proposal_revisions`; this lists them newest-first
 * with the editor + timestamp, and expands to a diff against the prior revision.
 */
export function ProposalHistory({ detail }: { detail: ProposalDetail }) {
  const proposal = detail.proposal!;
  const usersQuery = useQuery(usersQueryOptions());
  const userFor = (id?: string | null) =>
    id ? (usersQuery.data ?? []).find((u: OrgUser) => u.id === id) : undefined;

  const history = useMemo(
    () =>
      [...detail.revisions].sort(
        (a, b) =>
          b.seq - a.seq ||
          b.created_at.localeCompare(a.created_at) ||
          b.id.localeCompare(a.id),
      ),
    [detail.revisions],
  );
  const specRevisions = useMemo(
    () => detail.revisions.filter((r) => r.event_kind === "spec_revision"),
    [detail.revisions],
  );
  // Each spec revision diffs against the one immediately before it (seq − 1).
  const bySeq = useMemo(() => {
    const m = new Map<number, (typeof specRevisions)[number]>();
    specRevisions.forEach((r) => m.set(r.seq, r));
    return m;
  }, [specRevisions]);

  const [open, setOpen] = useState<string[]>([]);
  const toggle = (id: string) =>
    setOpen((cur) =>
      cur.includes(id) ? cur.filter((s) => s !== id) : [...cur, id],
    );

  // A lone seed revision has nothing to compare against yet, unless there are
  // non-spec audit events to show.
  if (history.length <= 1) return null;

  return (
    <div className="space-y-2">
      <Label className="text-xs uppercase text-muted-foreground">
        Revision history
      </Label>
      <ul className="divide-y rounded-md border">
        {history.map((r) => {
          const prev = bySeq.get(r.seq - 1);
          const editor = userFor(r.edited_by_user_id);
          const isHead = r.seq === proposal.latest_revision_seq;
          const isOpen = open.includes(r.id);
          const isSpecRevision = r.event_kind === "spec_revision";
          const titleChanged = !!prev && prev.title !== r.title;
          if (!isSpecRevision) {
            return (
              <li key={r.id}>
                <div className="flex w-full items-center gap-2 px-3 py-2 text-sm">
                  <Badge variant="secondary" className="font-mono">
                    status
                  </Badge>
                  <span className="text-muted-foreground">changed</span>
                  <span className="font-medium">{r.status_from ?? "—"}</span>
                  <span className="text-muted-foreground">→</span>
                  <span className="font-medium">{r.status_to ?? "—"}</span>
                  <span className="flex min-w-0 items-center gap-1.5">
                    <UserAvatar user={editor} className="size-4" />
                    <span className="truncate">
                      {editor
                        ? userDisplayName(editor)
                        : r.edited_by_user_id
                          ? "unknown"
                          : "—"}
                    </span>
                  </span>
                  <span className="ml-auto shrink-0 text-xs text-muted-foreground">
                    {relativeTime(r.created_at)}
                  </span>
                </div>
              </li>
            );
          }
          return (
            <li key={r.id}>
              <button
                onClick={() => toggle(r.id)}
                className="flex w-full items-center gap-2 px-3 py-2 text-left text-sm hover:bg-muted/40"
              >
                <HugeiconsIcon
                  icon={ArrowDown01Icon}
                  size={14}
                  className={cn(
                    "shrink-0 text-muted-foreground transition-transform",
                    isOpen && "rotate-180",
                  )}
                />
                <Badge
                  variant={isHead ? "default" : "outline"}
                  className="font-mono"
                >
                  rev {r.seq}
                </Badge>
                {isHead && (
                  <span className="text-xs text-muted-foreground">current</span>
                )}
                <span className="flex min-w-0 items-center gap-1.5">
                  <UserAvatar user={editor} className="size-4" />
                  <span className="truncate">
                    {editor
                      ? userDisplayName(editor)
                      : r.edited_by_user_id
                        ? "unknown"
                        : "—"}
                  </span>
                </span>
                <span className="ml-auto shrink-0 text-xs text-muted-foreground">
                  {relativeTime(r.created_at)}
                </span>
              </button>
              {isOpen && (
                <div className="space-y-2 px-3 pb-3">
                  {titleChanged && (
                    <p className="text-xs text-muted-foreground">
                      Title:{" "}
                      <span className="line-through">{prev.title}</span>{" "}
                      → <span className="text-foreground">{r.title}</span>
                    </p>
                  )}
                  <DiffView before={prev?.body ?? ""} after={r.body} />
                  {!prev && (
                    <p className="text-xs text-muted-foreground">
                      Initial spec.
                    </p>
                  )}
                </div>
              )}
            </li>
          );
        })}
      </ul>
    </div>
  );
}
