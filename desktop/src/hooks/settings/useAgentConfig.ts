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
    toggleRoleForModel,
    updateMaxSessions,
    memoryModel,
    setMemoryModel,
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
    onToggleRole: toggleRoleForModel,
    onUpdateMaxSessions: updateMaxSessions,
    memoryModel,
    onSetMemoryModel: setMemoryModel,
    onDismissError: resetError,
    onSave: () => void saveSettings(),
  };
}
