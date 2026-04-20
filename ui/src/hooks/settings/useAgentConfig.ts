import { useEffect } from 'react';
import { useSettingsStore } from '@/stores/settingsStore';

export function useAgentConfig() {
  const {
    models,
    availableModels,
    isLoading,
    isSaving,
    error,
    hasUnsavedChanges,
    loadSettings,
    loadProviderModels,
    addModel,
    removeModel,
    reorderModels,
    updateMaxSessions,
    saveSettings,
    resetError,
  } = useSettingsStore();

  useEffect(() => {
    void loadSettings();
    void loadProviderModels();
  }, [loadSettings, loadProviderModels]);

  return {
    models,
    availableModels,
    isLoading,
    isSaving,
    error,
    hasUnsavedChanges,
    onAddModel: addModel,
    onRemoveModel: removeModel,
    onReorderModels: reorderModels,
    onUpdateMaxSessions: (index: number, maxConcurrent: number) => updateMaxSessions(index, maxConcurrent),
    onDismissError: resetError,
    onSave: () => void saveSettings(),
  };
}
