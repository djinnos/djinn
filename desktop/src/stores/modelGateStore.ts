import { create } from 'zustand';
import { fetchSettings } from '@/api/settings';

interface ModelGateState {
  /** null = not yet checked */
  hasModels: boolean | null;
  refresh: () => Promise<void>;
}

export const useModelGateStore = create<ModelGateState>((set) => ({
  hasModels: null,

  refresh: async () => {
    try {
      const settings = await fetchSettings();
      set({ hasModels: settings.models.length > 0 });
    } catch {
      // On error leave the gate open so we don't block the user indefinitely
      set({ hasModels: true });
    }
  },
}));
