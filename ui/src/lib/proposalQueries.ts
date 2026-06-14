/**
 * TanStack Query options for the global Proposals layer.
 *
 * Proposals are project-independent, so the list is global (no project arg).
 * The detail query bundles the proposal, its target projects, and the full
 * feedback thread (one `proposal_show` call).
 */

import { queryOptions } from "@tanstack/react-query";
import { callMcpTool } from "@/api/mcpClient";
import type {
  Proposal,
  ProposalEpic,
  ProposalFeedback,
  ProposalRevision,
  ProposalSignoff,
  ProposalTarget,
} from "@/api/types";

export interface ProposalListFilters {
  status?: string;
  text?: string;
  target_project?: string;
}

export function proposalListQueryOptions(filters: ProposalListFilters = {}) {
  return queryOptions({
    queryKey: ["proposals", "list", filters] as const,
    queryFn: async () => {
      const res = await callMcpTool("proposal_list", {
        status: filters.status,
        text: filters.text,
        target_project: filters.target_project,
        sort: "created_desc",
        limit: 200,
      });
      return (res.proposals ?? []) as Proposal[];
    },
    staleTime: 30_000,
  });
}

export interface ProposalDetail {
  proposal: Proposal | null;
  targets: ProposalTarget[];
  feedback: ProposalFeedback[];
  /**
   * Chronological proposal history rows. Spec-revision fields remain present;
   * status-history metadata is optional for non-spec lifecycle events.
   */
  revisions: ProposalRevision[];
  signoffs: ProposalSignoff[];
  epics: ProposalEpic[];
}

export function proposalDetailQueryOptions(id: string | null) {
  return queryOptions({
    queryKey: ["proposals", "detail", id] as const,
    enabled: !!id,
    queryFn: async (): Promise<ProposalDetail> => {
      const res = await callMcpTool("proposal_show", { id: id as string });
      return {
        proposal: (res.proposal ?? null) as Proposal | null,
        targets: (res.targets ?? []) as ProposalTarget[],
        feedback: (res.feedback ?? []) as ProposalFeedback[],
        revisions: (res.revisions ?? []) as ProposalRevision[],
        signoffs: (res.signoffs ?? []) as ProposalSignoff[],
        epics: (res.epics ?? []) as ProposalEpic[],
      };
    },
    staleTime: 15_000,
  });
}
