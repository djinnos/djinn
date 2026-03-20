import { create } from 'zustand';
import {
  fetchSettings,
  saveSettings,
  fetchProviderModels,
  AgentRole,
  ModelPriorityItem,
  ModelSessionLimit,
  ProviderModel
} from '@/api/settings';

export interface UnifiedModelEntry {
  model: string;
  provider: string;
  enabledRoles: AgentRole[];
  max_concurrent: number;
  current_active: number;
}

export interface SettingsState {
  models: UnifiedModelEntry[];
  availableModels: ProviderModel[];
  isLoading: boolean;
  isSaving: boolean;
  error: string | null;
  hasUnsavedChanges: boolean;
}

export interface SettingsActions {
  loadSettings: () => Promise<void>;
  loadProviderModels: () => Promise<void>;
  addModel: (model: ModelPriorityItem) => void;
  removeModel: (index: number) => void;
  reorderModels: (fromIndex: number, toIndex: number) => void;
  toggleRoleForModel: (index: number, role: AgentRole) => void;
  updateMaxSessions: (index: number, maxConcurrent: number) => void;
  removeModelsByProvider: (provider: string) => void;
  saveSettings: () => Promise<void>;
  resetError: () => void;
}

const ALL_ROLES: AgentRole[] = ['worker', 'reviewer', 'lead', 'planner'];

function mergeToUnified(
  priorities: Record<AgentRole, ModelPriorityItem[]>,
  sessionLimits: ModelSessionLimit[],
): UnifiedModelEntry[] {
  const entries: UnifiedModelEntry[] = [];
  const seen = new Map<string, number>(); // key -> index in entries

  for (const role of ALL_ROLES) {
    for (const item of priorities[role]) {
      const key = `${item.provider}::${item.model}`;
      const idx = seen.get(key);
      if (idx !== undefined) {
        if (!entries[idx].enabledRoles.includes(role)) {
          entries[idx].enabledRoles.push(role);
        }
      } else {
        const limit = sessionLimits.find(
          (sl) => sl.model === item.model && sl.provider === item.provider
        );
        seen.set(key, entries.length);
        entries.push({
          model: item.model,
          provider: item.provider,
          enabledRoles: [role],
          max_concurrent: limit?.max_concurrent ?? 1,
          current_active: limit?.current_active ?? 0,
        });
      }
    }
  }
  return entries;
}

function splitFromUnified(models: UnifiedModelEntry[]): {
  priorities: Record<AgentRole, ModelPriorityItem[]>;
  sessionLimits: ModelSessionLimit[];
} {
  const priorities: Record<AgentRole, ModelPriorityItem[]> = {
    worker: [],
    reviewer: [],
    lead: [],
    planner: [],
  };

  const sessionLimits: ModelSessionLimit[] = [];

  for (const entry of models) {
    for (const role of ALL_ROLES) {
      if (entry.enabledRoles.includes(role)) {
        priorities[role].push({ model: entry.model, provider: entry.provider });
      }
    }
    sessionLimits.push({
      model: entry.model,
      provider: entry.provider,
      max_concurrent: entry.max_concurrent,
      current_active: entry.current_active,
    });
  }

  return { priorities, sessionLimits };
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
      const models = mergeToUnified(
        settings.agents.model_priorities,
        settings.agents.session_limits,
      );
      set({ models, isLoading: false, hasUnsavedChanges: false });
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

  addModel: (model: ModelPriorityItem) => {
    const { models } = get();
    const exists = models.some(
      (m) => m.model === model.model && m.provider === model.provider
    );
    if (exists) return;

    set({
      models: [
        ...models,
        {
          model: model.model,
          provider: model.provider,
          enabledRoles: [...ALL_ROLES],
          max_concurrent: 1,
          current_active: 0,
        },
      ],
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

  toggleRoleForModel: (index: number, role: AgentRole) => {
    const { models } = get();
    const entry = models[index];
    if (!entry) return;

    const hasRole = entry.enabledRoles.includes(role);
    const newRoles = hasRole
      ? entry.enabledRoles.filter((r) => r !== role)
      : [...entry.enabledRoles, role];

    const newModels = [...models];
    newModels[index] = { ...entry, enabledRoles: newRoles };
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
    const { priorities, sessionLimits } = splitFromUnified(models);

    set({ isSaving: true, error: null });
    try {
      await saveSettings({
        agents: {
          model_priorities: priorities,
          session_limits: sessionLimits,
        },
      });
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
