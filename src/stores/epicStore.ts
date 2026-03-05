/**
 * Epic Store - Vanilla createStore for epic state management
 * 
 * Uses subscribeWithSelector for granular subscriptions.
 * Updated directly from SSE event payloads (full-entity events).
 */

import { createStore } from "zustand/vanilla";
import { subscribeWithSelector } from "zustand/middleware";
import type { Epic, EpicCreatedPayload, EpicUpdatedPayload } from "../types";

export interface EpicState {
  // Epics map for O(1) lookups
  epics: Map<string, Epic>;
  
  // CRUD operations
  addEpic: (payload: EpicCreatedPayload) => void;
  updateEpic: (payload: EpicUpdatedPayload) => void;
  removeEpic: (id: string) => void;
  
  // Utility operations
  getEpic: (id: string) => Epic | undefined;
  getEpicsByStatus: (status: Epic['status']) => Epic[];
  getAllEpics: () => Epic[];
  clearEpics: () => void;
  
  // Batch operations
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
        newEpics.set(payload.id, payload);
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
