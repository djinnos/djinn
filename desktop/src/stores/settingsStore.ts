import { create } from 'zustand';
import {
  fetchSettings,
  saveSettings,
  saveLangfuseSettings,
  fetchProviderModels,
  ModelEntry,
  LangfuseSettings,
  ProviderModel
} from '@/api/settings';

export type { ModelEntry };

export interface SettingsState {
  models: ModelEntry[];
  availableModels: ProviderModel[];
  langfuse: LangfuseSettings;
  isLoading: boolean;
  isSaving: boolean;
  isSavingLangfuse: boolean;
  error: string | null;
  hasUnsavedChanges: boolean;
}

export interface SettingsActions {
  loadSettings: () => Promise<void>;
  loadProviderModels: () => Promise<void>;
  addModel: (model: { model: string; provider: string }) => void;
  removeModel: (index: number) => void;
  reorderModels: (fromIndex: number, toIndex: number) => void;
  updateMaxSessions: (indexOrModelId: number | string, maxConcurrent: number) => void;
  toggleRoleForModel: (indexOrModelId: number | string, role: string) => void;
  removeModelsByProvider: (provider: string) => void;
  saveSettings: () => Promise<boolean>;
  updateLangfuse: (langfuse: LangfuseSettings) => void;
  saveLangfuse: () => Promise<boolean>;
  resetError: () => void;
}

const initialState: SettingsState = {
  models: [],
  availableModels: [],
  langfuse: { publicKey: "", secretKey: "", endpoint: "" },
  isLoading: false,
  isSaving: false,
  isSavingLangfuse: false,
  error: null,
  hasUnsavedChanges: false,
};

export const useSettingsStore = create<SettingsState & SettingsActions>((set, get) => ({
  ...initialState,

  loadSettings: async () => {
    set({ isLoading: true, error: null });
    try {
      const settings = await fetchSettings();
      set({ models: settings.models, langfuse: settings.langfuse, isLoading: false, hasUnsavedChanges: false });
    } catch (error) {
      set({
        error: error instanceof Error ? error.message : 'Failed to load settings',
        isLoading: false
      });
    }
  },

  loadProviderModels: async () => {
    try {
      const models = await fetchProviderModels();
      set({ availableModels: models });
    } catch (error) {
      set({
        error: error instanceof Error ? error.message : 'Failed to load provider models'
      });
    }
  },

  addModel: (model) => {
    const { models } = get();
    const exists = models.some(
      (m) => m.model === model.model && m.provider === model.provider
    );
    if (exists) return;

    set({
      models: [...models, { model: model.model, provider: model.provider, max_concurrent: 1 }],
      hasUnsavedChanges: true,
    });
  },

  removeModel: (index: number) => {
    const { models } = get();
    set({
      models: models.filter((_, i) => i !== index),
      hasUnsavedChanges: true,
    });
  },

  reorderModels: (fromIndex: number, toIndex: number) => {
    const { models } = get();
    const newModels = [...models];
    const [moved] = newModels.splice(fromIndex, 1);
    newModels.splice(toIndex, 0, moved);
    set({ models: newModels, hasUnsavedChanges: true });
  },

  updateMaxSessions: (indexOrModelId: number | string, maxConcurrent: number) => {
    const { models } = get();
    const index = typeof indexOrModelId === 'number'
      ? indexOrModelId
      : models.findIndex((m) => m.model === indexOrModelId || `${m.provider}/${m.model}` === indexOrModelId);
    const entry = models[index];
    if (!entry) return;

    const newModels = [...models];
    newModels[index] = { ...entry, max_concurrent: maxConcurrent };
    set({ models: newModels, hasUnsavedChanges: true });
  },

  toggleRoleForModel: (indexOrModelId: number | string, _role: string) => {
    const { models } = get();
    const index = typeof indexOrModelId === 'number'
      ? indexOrModelId
      : models.findIndex((m) => m.model === indexOrModelId || `${m.provider}/${m.model}` === indexOrModelId);
    if (index < 0 || !models[index]) return;
    // Role toggling is a no-op at the store level for now (roles are managed server-side)
    set({ hasUnsavedChanges: true });
  },

  removeModelsByProvider: (provider: string) => {
    const { models } = get();
    set({
      models: models.filter((m) => m.provider !== provider),
      hasUnsavedChanges: true,
    });
  },

  saveSettings: async () => {
    const { models, langfuse } = get();

    set({ isSaving: true, error: null });
    try {
      await saveSettings({ models, langfuse });
      set({ isSaving: false, hasUnsavedChanges: false });
      return true;
    } catch (error) {
      const message = error instanceof Error ? error.message : 'Failed to save settings';
      console.error('[settings] save failed:', message);
      set({
        error: message,
        isSaving: false
      });
      return false;
    }
  },

  updateLangfuse: (langfuse: LangfuseSettings) => {
    set({ langfuse });
  },

  saveLangfuse: async () => {
    const { langfuse } = get();
    set({ isSavingLangfuse: true, error: null });
    try {
      await saveLangfuseSettings(langfuse);
      set({ isSavingLangfuse: false });
      return true;
    } catch (error) {
      const message = error instanceof Error ? error.message : 'Failed to save Langfuse settings';
      set({ error: message, isSavingLangfuse: false });
      return false;
    }
  },

  resetError: () => set({ error: null }),
}));
