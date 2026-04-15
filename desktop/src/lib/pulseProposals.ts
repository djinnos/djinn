import { queryOptions } from "@tanstack/react-query";
import type { User } from "@/api/auth";

type UserLike = Pick<User, "id" | "login" | "name"> | { sub?: string; email?: string; name?: string } | null | undefined;
import { callMcpTool } from "@/api/mcpClient";
import type { ProposeAdrListOutput } from "@/api/generated/mcp-tools.gen";

export type PulseProposalSummary = NonNullable<ProposeAdrListOutput["items"]>[number] & {
  modifiedAt: string | null;
};

const pulseSpikeOriginators = new Map<string, string>();
const notifiedDraftIds = new Set<string>();

export function parseProposalItems(output: ProposeAdrListOutput): PulseProposalSummary[] {
  return (output.items ?? []).map((item) => ({
    ...item,
    modifiedAt: typeof item.mtime === "string" && item.mtime.length > 0 ? item.mtime : null,
  }));
}

export function pulseProposalListQueryOptions(projectPath: string) {
  return queryOptions({
    queryKey: ["pulse", "architect-proposals", projectPath],
    queryFn: async () => parseProposalItems(await callMcpTool("propose_adr_list", { project: projectPath })),
    enabled: !!projectPath,
    staleTime: 30_000,
    refetchInterval: 30_000,
    refetchOnWindowFocus: true,
  });
}

function originatorKey(user: UserLike): string | null {
  if (!user) return null;
  const anyUser = user as Record<string, string | null | undefined>;
  return (
    anyUser.id ??
    anyUser.login ??
    anyUser.sub ??
    anyUser.email ??
    anyUser.name ??
    null
  );
}

export function recordPulseOriginatedSpike(spikeId: string, user: UserLike): void {
  const key = originatorKey(user);
  if (!spikeId || !key) return;
  pulseSpikeOriginators.set(spikeId, key);
}

export function shouldNotifyForProposalDraft(
  proposal: Pick<PulseProposalSummary, "id" | "originating_spike_id">,
  user: UserLike,
): boolean {
  const key = originatorKey(user);
  if (!key || !proposal.id || !proposal.originating_spike_id) return false;
  if (notifiedDraftIds.has(proposal.id)) return false;
  return pulseSpikeOriginators.get(proposal.originating_spike_id) === key;
}

export function markProposalDraftNotified(draftId: string): void {
  if (draftId) {
    notifiedDraftIds.add(draftId);
  }
}
