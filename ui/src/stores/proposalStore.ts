/**
 * Proposal Store - Vanilla createStore for the global proposals layer.
 *
 * Mirrors epicStore: a Map keyed by id, updated directly from SSE
 * full-entity events. Proposals are global (no project scope).
 */

import { createStore } from "zustand/vanilla";
import { subscribeWithSelector } from "zustand/middleware";
import type { Proposal } from "@/api/types";

export interface ProposalState {
  proposals: Map<string, Proposal>;
  addProposal: (proposal: Proposal) => void;
  updateProposal: (proposal: Proposal) => void;
  removeProposal: (id: string) => void;
  getProposal: (id: string) => Proposal | undefined;
  getAllProposals: () => Proposal[];
  setProposals: (proposals: Proposal[]) => void;
  clearProposals: () => void;
}

export const proposalStore = createStore<ProposalState>()(
  subscribeWithSelector((set, get) => ({
    proposals: new Map(),

    addProposal: (payload) => {
      set((state) => {
        const next = new Map(state.proposals);
        next.set(payload.id, payload);
        return { proposals: next };
      });
    },

    updateProposal: (payload) => {
      set((state) => {
        const next = new Map(state.proposals);
        next.set(payload.id, payload);
        return { proposals: next };
      });
    },

    removeProposal: (id) => {
      set((state) => {
        const next = new Map(state.proposals);
        next.delete(id);
        return { proposals: next };
      });
    },

    getProposal: (id) => get().proposals.get(id),

    getAllProposals: () => Array.from(get().proposals.values()),

    setProposals: (proposals) => {
      set({ proposals: new Map(proposals.map((p) => [p.id, p])) });
    },

    clearProposals: () => set({ proposals: new Map() }),
  }))
);
