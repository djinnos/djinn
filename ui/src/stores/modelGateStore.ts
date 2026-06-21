import { create } from 'zustand';
import { fetchUserSettings } from '@/api/userSettings';

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
      // THIS user has selected at least one model in ANY role lane.
      const settings = await fetchUserSettings();
      const { plan, implement, review } = settings.lanes;
      set({ hasModels: plan.length + implement.length + review.length > 0 });
    } catch {
      // On error leave the gate open so we don't block the user indefinitely
      set({ hasModels: true });
    }
  },
}));
