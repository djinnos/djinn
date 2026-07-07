import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { callMcpTool } from "@/api/mcpClient";
import { usersQueryOptions } from "@/api/queryOptions";
import { userDisplayName, type OrgUser } from "@/api/users";
import { useAuthUser } from "@/components/AuthGate";
import { UserAvatar } from "@/components/UserAvatar";
import { DiffView } from "@/components/proposals/DiffView";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { showToast } from "@/lib/toast";
import { canSignoff, capsFromUser } from "@/lib/proposalPermissions";
import type { ProposalDetail } from "@/lib/proposalQueries";

const KINDS: { key: "scoped" | "technical"; label: string }[] = [
  { key: "scoped", label: "Scope (product)" },
  { key: "technical", label: "Technical (engineering)" },
];

export function ProposalSignoffs({
  detail,
  onChanged,
}: {
  detail: ProposalDetail;
  onChanged: () => void;
}) {
  const proposal = detail.proposal!;
  const me = useAuthUser();
  const caps = capsFromUser(me);
  const usersQuery = useQuery(usersQueryOptions());
  const userFor = (userId: string) =>
    (usersQuery.data ?? []).find((x: OrgUser) => x.id === userId);
  const nameFor = (userId: string) => {
    const u = userFor(userId);
    return u ? userDisplayName(u) : userId;
  };

  const sign = async (kind: string) => {
    try {
      const res = await callMcpTool("proposal_signoff", { id: proposal.id, kind });
      if (res.error) throw new Error(res.error);
      onChanged();
    } catch (e) {
      showToast.error("Sign-off failed", { description: (e as Error).message });
    }
  };
  const withdraw = async (kind: string) => {
    try {
      const res = await callMcpTool("proposal_signoff_clear", { id: proposal.id, kind });
      if (res.error) throw new Error(res.error);
      onChanged();
    } catch (e) {
      showToast.error("Failed to withdraw", { description: (e as Error).message });
    }
  };

  // "Changes since your sign-off": diff the revision the current user last
  // signed off against vs. the head revision.
  const myStaleAnchor = useMemo(() => {
    if (!me) return null;
    const mine = detail.signoffs.filter((s) => s.user_id === me.id && s.stale);
    if (mine.length === 0) return null;
    const seq = Math.min(...mine.map((s) => s.revision_seq));
    const from = detail.revisions.find((r) => r.seq === seq);
    const head = detail.revisions.find((r) => r.seq === proposal.latest_revision_seq);
    if (!from || !head) return null;
    return { from, head };
  }, [me, detail.signoffs, detail.revisions, proposal.latest_revision_seq]);

  return (
    <div className="space-y-4">
      <div className="space-y-3 rounded-md border p-3">
        <div className="flex items-center justify-between">
          <Label className="text-xs uppercase text-muted-foreground">Sign-offs</Label>
          <span className="text-xs text-muted-foreground">
            Approved when scope + technical are both fresh
          </span>
        </div>
        {KINDS.map(({ key, label }) => {
          const signers = detail.signoffs.filter((s) => s.kind === key);
          const mine = me ? signers.find((s) => s.user_id === me.id) : undefined;
          return (
            <div key={key} className="flex items-center justify-between gap-3">
              <div className="flex min-w-0 flex-wrap items-center gap-2">
                <span className="text-sm font-medium">{label}</span>
                {signers.length === 0 && (
                  <span className="text-xs text-muted-foreground">— not signed off</span>
                )}
                {signers.map((s) => (
                  <Badge
                    key={s.user_id}
                    variant={s.stale ? "outline" : "default"}
                    className="gap-1.5 pl-1"
                  >
                    <UserAvatar user={userFor(s.user_id)} className="size-4" />
                    {nameFor(s.user_id)}
                    {s.stale && <span className="text-amber-500">stale</span>}
                  </Badge>
                ))}
              </div>
              {mine && !mine.stale ? (
                <Button size="sm" variant="ghost" onClick={() => withdraw(key)}>
                  Withdraw
                </Button>
              ) : canSignoff(caps, key) ? (
                <Button size="sm" variant="outline" onClick={() => sign(key)}>
                  {mine?.stale ? "Re-sign" : "Sign off"}
                </Button>
              ) : null}
            </div>
          );
        })}
      </div>

      {myStaleAnchor && (
        <div className="space-y-2">
          <Label className="text-xs uppercase text-amber-500">
            Changes since your sign-off (rev {myStaleAnchor.from.seq} → {myStaleAnchor.head.seq})
          </Label>
          <DiffView before={myStaleAnchor.from.body ?? ""} after={myStaleAnchor.head.body ?? ""} />
        </div>
      )}
    </div>
  );
}
