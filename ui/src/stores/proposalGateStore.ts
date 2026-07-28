import { create } from "zustand";

import { hasAnyProposal } from "@/api/proposals";

interface ProposalGateState {
  /** null = not yet checked */
  hasProposal: boolean | null;
  error: string | null;
  refresh: () => Promise<void>;
  markComplete: () => void;
}

export const useProposalGateStore = create<ProposalGateState>((set) => ({
  hasProposal: null,
  error: null,

  refresh: async () => {
    try {
      set({ hasProposal: await hasAnyProposal(), error: null });
    } catch (error) {
      // The first-proposal step is required setup. An API failure must not
      // silently open an empty application shell.
      set({
        hasProposal: null,
        error:
          error instanceof Error
            ? error.message
            : "Could not check proposal setup",
      });
    }
  },

  markComplete: () => set({ hasProposal: true, error: null }),
}));
