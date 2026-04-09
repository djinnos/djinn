import { queryOptions } from "@tanstack/react-query";
import type { AuthUser } from "@/electron/commands";
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
    modifiedAt: null,
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

function originatorKey(user: AuthUser | null | undefined): string | null {
  if (!user) return null;
  return user.sub ?? user.email ?? user.name ?? null;
}

export function recordPulseOriginatedSpike(spikeId: string, user: AuthUser | null | undefined): void {
  const key = originatorKey(user);
  if (!spikeId || !key) return;
  pulseSpikeOriginators.set(spikeId, key);
}

export function shouldNotifyForProposalDraft(
  proposal: Pick<PulseProposalSummary, "id" | "originating_spike_id">,
  user: AuthUser | null | undefined,
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
