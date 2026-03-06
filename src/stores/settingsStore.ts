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

export interface SettingsState {
  // Data
  modelPriorities: Record<AgentRole, ModelPriorityItem[]>;
  sessionLimits: ModelSessionLimit[];
  availableModels: ProviderModel[];
  
  // Status
  isLoading: boolean;
  isSaving: boolean;
  error: string | null;
  hasUnsavedChanges: boolean;
}

export interface SettingsActions {
  // Load actions
  loadSettings: () => Promise<void>;
  loadProviderModels: () => Promise<void>;
  
  // Model priority actions
  addModelToRole: (role: AgentRole, model: ModelPriorityItem) => void;
  removeModelFromRole: (role: AgentRole, index: number) => void;
  reorderModelsInRole: (role: AgentRole, fromIndex: number, toIndex: number) => void;
  
  // Session limit actions
  updateSessionLimit: (model: string, provider: string, maxConcurrent: number) => void;
  
  // Provider cleanup
  removeModelsByProvider: (provider: string) => void;

  // Save action
  saveSettings: () => Promise<void>;

  // Reset
  resetError: () => void;
}

const DEFAULT_MODEL_PRIORITIES: Record<AgentRole, ModelPriorityItem[]> = {
  worker: [],
  task_reviewer: [],
  epic_reviewer: [],
};

const initialState: SettingsState = {
  modelPriorities: DEFAULT_MODEL_PRIORITIES,
  sessionLimits: [],
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
      set({
        modelPriorities: settings.agents.model_priorities,
        sessionLimits: settings.agents.session_limits,
        isLoading: false,
        hasUnsavedChanges: false,
      });
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

  addModelToRole: (role: AgentRole, model: ModelPriorityItem) => {
    const { modelPriorities } = get();
    const currentList = modelPriorities[role];
    
    // Check if model already exists in this role
    const exists = currentList.some(
      item => item.model === model.model && item.provider === model.provider
    );
    
    if (exists) return;
    
    set({
      modelPriorities: {
        ...modelPriorities,
        [role]: [...currentList, model],
      },
      hasUnsavedChanges: true,
    });
  },

  removeModelFromRole: (role: AgentRole, index: number) => {
    const { modelPriorities } = get();
    const currentList = modelPriorities[role];
    
    set({
      modelPriorities: {
        ...modelPriorities,
        [role]: currentList.filter((_, i) => i !== index),
      },
      hasUnsavedChanges: true,
    });
  },

  reorderModelsInRole: (role: AgentRole, fromIndex: number, toIndex: number) => {
    const { modelPriorities } = get();
    const currentList = [...modelPriorities[role]];
    
    const [movedItem] = currentList.splice(fromIndex, 1);
    currentList.splice(toIndex, 0, movedItem);
    
    set({
      modelPriorities: {
        ...modelPriorities,
        [role]: currentList,
      },
      hasUnsavedChanges: true,
    });
  },

  updateSessionLimit: (model: string, provider: string, maxConcurrent: number) => {
    const { sessionLimits } = get();
    
    const existingIndex = sessionLimits.findIndex(
      item => item.model === model && item.provider === provider
    );
    
    let newSessionLimits: ModelSessionLimit[];
    
    if (existingIndex >= 0) {
      newSessionLimits = sessionLimits.map((item, i) =>
        i === existingIndex ? { ...item, max_concurrent: maxConcurrent } : item
      );
    } else {
      newSessionLimits = [
        ...sessionLimits,
        { model, provider, max_concurrent: maxConcurrent, current_active: 0 },
      ];
    }
    
    set({
      sessionLimits: newSessionLimits,
      hasUnsavedChanges: true,
    });
  },

  removeModelsByProvider: (provider: string) => {
    const { modelPriorities, sessionLimits } = get();
    const roles: AgentRole[] = ['worker', 'task_reviewer', 'epic_reviewer'];
    const newPriorities = { ...modelPriorities };
    for (const role of roles) {
      newPriorities[role] = modelPriorities[role].filter(
        (item) => item.provider !== provider
      );
    }
    set({
      modelPriorities: newPriorities,
      sessionLimits: sessionLimits.filter((sl) => sl.provider !== provider),
      hasUnsavedChanges: true,
    });
  },

  saveSettings: async () => {
    const { modelPriorities, sessionLimits } = get();
    
    set({ isSaving: true, error: null });
    try {
      await saveSettings({
        agents: {
          model_priorities: modelPriorities,
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
