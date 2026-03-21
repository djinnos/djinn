import { create } from 'zustand';
import {
  fetchSettings,
  saveSettings,
  fetchProviderModels,
  ModelEntry,
  ProviderModel
} from '@/api/settings';

export type { ModelEntry };

export interface SettingsState {
  models: ModelEntry[];
  availableModels: ProviderModel[];
  isLoading: boolean;
  isSaving: boolean;
  error: string | null;
  hasUnsavedChanges: boolean;
}

export interface SettingsActions {
  loadSettings: () => Promise<void>;
  loadProviderModels: () => Promise<void>;
  addModel: (model: { model: string; provider: string }) => void;
  removeModel: (index: number) => void;
  reorderModels: (fromIndex: number, toIndex: number) => void;
  updateMaxSessions: (index: number, maxConcurrent: number) => void;
  removeModelsByProvider: (provider: string) => void;
  saveSettings: () => Promise<void>;
  resetError: () => void;
}

const initialState: SettingsState = {
  models: [],
  availableModels: [],
  isLoading: false,
  isSaving: false,
  error: null,
  hasUnsavedChanges: false,
};

export const useSettingsStore = create<SettingsState & SettingsActions>((set, get) => ({
  ...initialState,

  loadSettings: async () => {
    set({ isLoading: true, error: null });
    try {
      const settings = await fetchSettings();
      set({ models: settings.models, isLoading: false, hasUnsavedChanges: false });
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

  updateMaxSessions: (index: number, maxConcurrent: number) => {
    const { models } = get();
    const entry = models[index];
    if (!entry) return;

    const newModels = [...models];
    newModels[index] = { ...entry, max_concurrent: maxConcurrent };
    set({ models: newModels, hasUnsavedChanges: true });
  },

  removeModelsByProvider: (provider: string) => {
    const { models } = get();
    set({
      models: models.filter((m) => m.provider !== provider),
      hasUnsavedChanges: true,
    });
  },

  saveSettings: async () => {
    const { models } = get();

    set({ isSaving: true, error: null });
    try {
      await saveSettings({ models });
      set({ isSaving: false, hasUnsavedChanges: false });
    } catch (error) {
      set({
        error: error instanceof Error ? error.message : 'Failed to save settings',
        isSaving: false
      });
    }
  },

  resetError: () => set({ error: null }),
}));
