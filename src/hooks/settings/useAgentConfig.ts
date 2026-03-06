import { useEffect } from 'react';
import { useWizardStore } from '@/stores/wizardStore';
import { useSettingsStore } from '@/stores/settingsStore';

export function useAgentConfig() {
  const { resetWizard } = useWizardStore();
  const {
    modelPriorities,
    sessionLimits,
    availableModels,
    isLoading,
    isSaving,
    error,
    hasUnsavedChanges,
    loadSettings,
    loadProviderModels,
    addModelToRole,
    removeModelFromRole,
    reorderModelsInRole,
    updateSessionLimit,
    saveSettings,
    resetError,
  } = useSettingsStore();

  useEffect(() => {
    void loadSettings();
    void loadProviderModels();
  }, [loadSettings, loadProviderModels]);

  const handleResetWizard = () => {
    if (confirm('Are you sure you want to reset the wizard? This will show the setup wizard on next launch.')) {
      resetWizard();
    }
  };

  return {
    handleResetWizard,
    modelPriorities,
    sessionLimits,
    availableModels,
    isLoading,
    isSaving,
    error,
    hasUnsavedChanges,
    loadSettings,
    loadProviderModels,
    addModelToRole,
    removeModelFromRole,
    reorderModelsInRole,
    updateSessionLimit,
    saveSettings,
    resetError,
    onAddModel: addModelToRole,
    onRemoveModel: removeModelFromRole,
    onReorderModels: reorderModelsInRole,
    onUpdateSessionLimit: updateSessionLimit,
    onDismissError: resetError,
    onSave: () => void saveSettings(),
  };
}
