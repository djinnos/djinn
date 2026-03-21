import { useEffect } from 'react';
import { useWizardStore } from '@/stores/wizardStore';
import { useSettingsStore } from '@/stores/settingsStore';

export function useAgentConfig() {
  const { resetWizard } = useWizardStore();
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

  const handleResetWizard = () => {
    resetWizard();
  };

  return {
    handleResetWizard,
    models,
    availableModels,
    isLoading,
    isSaving,
    error,
    hasUnsavedChanges,
    onAddModel: addModel,
    onRemoveModel: removeModel,
    onReorderModels: reorderModels,
    onUpdateMaxSessions: updateMaxSessions,
    onDismissError: resetError,
    onSave: () => void saveSettings(),
  };
}
