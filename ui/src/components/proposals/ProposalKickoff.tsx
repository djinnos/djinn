import { useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { callMcpTool } from "@/api/mcpClient";
import { usersQueryOptions } from "@/api/queryOptions";
import { userDisplayName, type OrgUser } from "@/api/users";
import { useAuthUser } from "@/components/AuthGate";
import { UserAvatar } from "@/components/UserAvatar";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { showToast } from "@/lib/toast";
import { canKickoff, capsFromUser } from "@/lib/proposalPermissions";
import type { ProposalDetail } from "@/lib/proposalQueries";

/**
 * Kick-off (graduation) control and graduated-epic list. Shown on an approved
 * proposal for engineers/admins; once building, lists the spawned epics.
 */
export function ProposalKickoff({
  detail,
  onChanged,
}: {
  detail: ProposalDetail;
  onChanged: () => void;
}) {
  const proposal = detail.proposal!;
  const navigate = useNavigate();
  const me = useAuthUser();
  const caps = capsFromUser(me);
  const usersQuery = useQuery(usersQueryOptions());
  const userFor = (id: string | null | undefined) =>
    id ? (usersQuery.data ?? []).find((x: OrgUser) => x.id === id) : undefined;
  const nameFor = (id: string | null | undefined) => {
    if (!id) return "unknown";
    const u = userFor(id);
    return u ? userDisplayName(u) : id;
  };

  const participants = useMemo(() => {
    const ids = new Set<string>();
    if (proposal.author_user_id) ids.add(proposal.author_user_id);
    detail.signoffs.forEach((s) => ids.add(s.user_id));
    return Array.from(ids);
  }, [proposal.author_user_id, detail.signoffs]);

  const [owner, setOwner] = useState<string>(
    me?.id && participants.includes(me.id) ? me.id : (participants[0] ?? ""),
  );
  const [busy, setBusy] = useState(false);

  const kickoff = async () => {
    setBusy(true);
    try {
      const res = await callMcpTool("proposal_graduate", {
        id: proposal.id,
        owner_user_id: owner || undefined,
      });
      if (res.error) throw new Error(res.error);
      showToast.success("Build kicked off");
      onChanged();
    } catch (e) {
      showToast.error("Kick-off failed", { description: (e as Error).message });
    } finally {
      setBusy(false);
    }
  };

  // Graduated → show the spawned epics.
  if (detail.epics.length > 0) {
    return (
      <div className="space-y-2 rounded-md border p-3">
        <div className="flex items-center justify-between">
          <Label className="text-xs uppercase text-muted-foreground">
            Graduated epics
          </Label>
          {proposal.build_owner_user_id && (
            <span className="flex items-center gap-1.5 text-xs text-muted-foreground">
              owned by
              <UserAvatar
                user={userFor(proposal.build_owner_user_id)}
                className="size-4"
              />
              {nameFor(proposal.build_owner_user_id)}
            </span>
          )}
        </div>
        <ul className="space-y-1">
          {detail.epics.map((e) => (
            <li key={e.epic_id}>
              <button
                type="button"
                onClick={() =>
                  navigate(`/tasks?epic=${encodeURIComponent(e.epic_id)}`)
                }
                className="flex w-full items-center gap-2 rounded px-1 py-0.5 text-left text-sm hover:bg-muted/50"
                title="View this epic and its tasks on the board"
              >
                <Badge variant="outline" className="font-mono">
                  {e.epic_short_id}
                </Badge>
                <span className="flex min-w-0 items-center gap-1.5">
                  <span className="shrink-0 leading-none">
                    {e.epic_emoji || "📌"}
                  </span>
                  <span className="truncate font-medium">{e.epic_title}</span>
                </span>
                <span className="shrink-0 text-muted-foreground">
                  {e.project_path}
                </span>
                <Badge variant="secondary" className="capitalize">
                  {e.status}
                </Badge>
                {e.needs_reconcile === true ? (
                  <Badge
                    variant="outline"
                    className="border-amber-500/40 bg-amber-500/10 text-amber-700 dark:text-amber-300"
                  >
                    needs reconcile
                  </Badge>
                ) : typeof e.reconciled_at_revision_seq === "number" ? (
                  <Badge variant="secondary" className="text-muted-foreground">
                    reconciled at rev {e.reconciled_at_revision_seq}
                  </Badge>
                ) : null}
              </button>
            </li>
          ))}
        </ul>
      </div>
    );
  }

  // Approved + kick-off capability → offer to build.
  if (proposal.status !== "approved" || !canKickoff(caps)) return null;

  return (
    <div className="space-y-3 rounded-md border border-primary/40 bg-primary/5 p-3">
      <Label className="text-xs uppercase text-muted-foreground">
        Ready to build
      </Label>
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-sm">Owner</span>
        <Select
          value={owner}
          onValueChange={(v) => typeof v === "string" && setOwner(v)}
        >
          <SelectTrigger className="h-8 w-[200px] text-sm">
            {/* Render the resolved name explicitly: `owner` is set
                programmatically, so Radix never captures the selected item's
                text and SelectValue would otherwise fall back to the raw id. */}
            <SelectValue placeholder="Pick a participant">
              {owner ? (
                <span className="flex items-center gap-2">
                  <UserAvatar user={userFor(owner)} className="size-4" />
                  {nameFor(owner)}
                </span>
              ) : undefined}
            </SelectValue>
          </SelectTrigger>
          <SelectContent>
            {participants.map((id) => (
              <SelectItem key={id} value={id}>
                <span className="flex items-center gap-2">
                  <UserAvatar user={userFor(id)} className="size-4" />
                  {nameFor(id)}
                </span>
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Button size="sm" disabled={busy || !owner} onClick={kickoff}>
          {busy ? "Kicking off…" : "Kick off build"}
        </Button>
      </div>
      <p className="text-xs text-muted-foreground">
        Hands the proposal to djinn's planner, which breaks it down into epics
        across the target repos and builds it.
      </p>
    </div>
  );
}
