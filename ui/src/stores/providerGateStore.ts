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
      // Provider connectivity is required for first-run setup. Fail closed so
      // the onboarding surface can show its own retryable API error instead of
      // silently opening an unusable app.
      set({ hasProvider: false });
    }
  },
}));
