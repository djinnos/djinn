import { create } from 'zustand';
import { fetchCredentialList } from '@/api/server';

interface ProviderGateState {
  /** null = not yet checked */
  hasProvider: boolean | null;
  refresh: () => Promise<void>;
}

export const useProviderGateStore = create<ProviderGateState>((set) => ({
  hasProvider: null,

  refresh: async () => {
    try {
      const credentials = await fetchCredentialList();
      set({ hasProvider: credentials.some((c) => c.valid) });
    } catch {
      // On error leave the gate open so we don't block the user indefinitely
      set({ hasProvider: true });
    }
  },
}));
