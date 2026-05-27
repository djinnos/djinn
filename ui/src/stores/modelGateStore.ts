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
      // The model list is now per-user: gate onboarding on whether THIS user
      // has selected at least one model.
      const settings = await fetchUserSettings();
      set({ hasModels: settings.models.length > 0 });
    } catch {
      // On error leave the gate open so we don't block the user indefinitely
      set({ hasModels: true });
    }
  },
}));
