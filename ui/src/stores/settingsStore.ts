import { create } from 'zustand';
import {
  fetchProviderModels,
  combineModelId,
  splitModelId,
  ModelEntry,
  ProviderModel,
} from '@/api/settings';
import {
  fetchUserSettings,
  patchUserSettings,
  lanesUnion,
  type ModelLanes,
} from '@/api/userSettings';
import { queryClient } from '@/lib/queryClient';
import { USER_SETTINGS_QUERY_KEY } from '@/api/queryOptions';

export type { ModelEntry };

export interface SettingsState {
  /**
   * The user's ordered model list (priority top → bottom). Both the order and
   * each `max_concurrent` cap are PER-USER, sourced from `user_settings`
   * (`models` + `max_sessions`). Every user edits their own caps.
   */
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
  updateMaxSessions: (indexOrModelId: number | string, maxConcurrent: number) => void;
  removeModelsByProvider: (provider: string) => void;
  saveSettings: () => Promise<boolean>;
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

/** Build the displayed `ModelEntry[]` from per-user model ids + per-user caps. */
function buildEntries(
  modelIds: string[],
  maxSessions: Record<string, number>,
): ModelEntry[] {
  const seen = new Set<string>();
  const entries: ModelEntry[] = [];
  for (const id of modelIds) {
    if (seen.has(id)) continue;
    seen.add(id);
    const { provider, model } = splitModelId(id);
    entries.push({
      provider,
      model,
      max_concurrent: maxSessions[id] ?? 1,
    });
  }
  return entries;
}

export const useSettingsStore = create<SettingsState & SettingsActions>((set, get) => ({
  ...initialState,

  loadSettings: async () => {
    set({ isLoading: true, error: null });
    try {
      // Per-user settings are the source of truth for both the model list and
      // each model's concurrency cap.
      const userSettings = await fetchUserSettings();
      // Keep the shared user-settings query in sync so the chat picker sees
      // the same selection without a second fetch.
      queryClient.setQueryData(USER_SETTINGS_QUERY_KEY, userSettings);
      set({
        // This legacy flat editor (used only by the first-run onboarding sheet
        // — Slice 4) edits a single list; surface the union across role lanes.
        models: buildEntries(lanesUnion(userSettings.lanes), userSettings.maxSessions),
        isLoading: false,
        hasUnsavedChanges: false,
      });
    } catch (error) {
      set({
        error: error instanceof Error ? error.message : 'Failed to load settings',
        isLoading: false,
      });
    }
  },

  loadProviderModels: async () => {
    try {
      const models = await fetchProviderModels();
      set({ availableModels: models });
    } catch (error) {
      set({
        error: error instanceof Error ? error.message : 'Failed to load provider models',
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
    // Per-model caps are PER-USER now — a normal unsaved change.
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
      // Per-user ordered model list + per-model caps — every signed-in user
      // edits their own. Persist both in a single patch so the order and the
      // caps stay in lockstep.
      const modelIds = models.map((m) => combineModelId(m.provider, m.model));
      const maxSessions: Record<string, number> = {};
      for (const m of models) {
        maxSessions[combineModelId(m.provider, m.model)] = m.max_concurrent;
      }
      // This flat editor has no per-role notion (per-role lanes are edited in
      // Settings → Model Roles). Cut-over: seed the same list into all three
      // lanes so this editor stays a valid view of the user's selection.
      const lanes: ModelLanes = {
        plan: modelIds,
        implement: modelIds,
        review: modelIds,
      };
      const next = await patchUserSettings({ lanes, maxSessions });
      queryClient.setQueryData(USER_SETTINGS_QUERY_KEY, next);
      set({ isSaving: false, hasUnsavedChanges: false });
      return true;
    } catch (error) {
      const message = error instanceof Error ? error.message : 'Failed to save settings';
      console.error('[settings] save failed:', message);
      set({
        error: message,
        isSaving: false,
      });
      return false;
    }
  },

  resetError: () => set({ error: null }),
}));
