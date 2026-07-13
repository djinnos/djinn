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
      // THIS user has an effective model in EVERY role lane. Locked org policy
      // still has to resolve all three lanes; a locked-but-empty policy cannot
      // dispatch work and must remain visibly blocked for an admin to fix.
      const settings = await fetchUserModelSelection(SELF_TARGET);
      set({
        hasModels: MODEL_LANE_KEYS.every(
          (lane) => settings.lanes[lane].length > 0,
        ),
      });
    } catch {
      // Required setup must not disappear because a readiness call failed.
      set({ hasModels: false });
    }
  },
}));
