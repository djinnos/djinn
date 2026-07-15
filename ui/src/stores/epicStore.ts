/**
 * Epic Store - Vanilla createStore for epic state management
 * 
 * Uses subscribeWithSelector for granular subscriptions.
 * Updated directly from SSE event payloads (full-entity events).
 */

import { createStore } from "zustand/vanilla";
import { subscribeWithSelector } from "zustand/middleware";
import type { Epic } from "@/api/types";

export interface EpicState {
  epics: Map<string, Epic>;
  addEpic: (epic: Epic) => void;
  updateEpic: (epic: Epic) => void;
  removeEpic: (id: string) => void;
  getEpic: (id: string) => Epic | undefined;
  getEpicsByStatus: (status: string) => Epic[];
  getAllEpics: () => Epic[];
  clearEpics: () => void;
  setEpics: (epics: Epic[]) => void;
}

export const epicStore = createStore<EpicState>()(
  subscribeWithSelector((set, get) => ({
    epics: new Map(),

    addEpic: (payload) => {
      set((state) => {
        const newEpics = new Map(state.epics);
        newEpics.set(payload.id, payload);
        return { epics: newEpics };
      });
    },

    updateEpic: (payload) => {
      set((state) => {
        const existingEpic = state.epics.get(payload.id);
        if (!existingEpic) return state;

        const newEpics = new Map(state.epics);
        // SSE epic payloads carry the proposal label enrichment
        // (short_id/title/status) just like epic_list rows. The server's
        // label hydration is fail-open, though: on a lookup failure it sends
        // the epic bare, so preserve the known labels unless the linkage
        // itself changed.
        const sameProposal = payload.proposal_id === existingEpic.proposal_id;
        newEpics.set(payload.id, {
          ...payload,
          proposal_short_id:
            payload.proposal_short_id ??
            (sameProposal ? existingEpic.proposal_short_id : null),
          proposal_title:
            payload.proposal_title ??
            (sameProposal ? existingEpic.proposal_title : null),
          proposal_status:
            payload.proposal_status ??
            (sameProposal ? existingEpic.proposal_status : null),
          proposal_build_owner_user_id:
            payload.proposal_build_owner_user_id ??
            (sameProposal ? existingEpic.proposal_build_owner_user_id : null),
        });
        return { epics: newEpics };
      });
    },

    removeEpic: (id) => {
      set((state) => {
        const newEpics = new Map(state.epics);
        newEpics.delete(id);
        return { epics: newEpics };
      });
    },

    getEpic: (id) => {
      return get().epics.get(id);
    },

    getEpicsByStatus: (status) => {
      return Array.from(get().epics.values()).filter(
        (epic) => epic.status === status
      );
    },

    getAllEpics: () => {
      return Array.from(get().epics.values());
    },

    clearEpics: () => {
      set({ epics: new Map() });
    },

    setEpics: (epics) => {
      set({
        epics: new Map(epics.map((epic) => [epic.id, epic])),
      });
    },
  }))
);

// React hook for components (with selector support)
export { useEpicStore } from "./useEpicStore";
