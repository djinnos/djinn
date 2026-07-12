import { create } from 'zustand';
import { fetchUserModelSelection, SELF_TARGET } from '@/api/userConfig';
import { MODEL_LANE_KEYS } from '@/api/userSettings';

interface ModelGateState {
  /** null = not yet checked */
  hasModels: boolean | null;
  refresh: () => Promise<void>;
}

export const useModelGateStore = create<ModelGateState>((set) => ({
  hasModels: null,

  refresh: async () => {
    try {
      // The model list is now per-user and per-role: gate onboarding on whether
      // THIS user has selected at least one model in EVERY role lane. A locked
      // org policy is complete by definition because the user cannot edit it.
      const settings = await fetchUserModelSelection(SELF_TARGET);
      set({
        hasModels:
          settings.laneLocked === true ||
          MODEL_LANE_KEYS.every((lane) => settings.lanes[lane].length > 0),
      });
    } catch {
      // On error leave the gate open so we don't block the user indefinitely
      set({ hasModels: true });
    }
  },
}));
