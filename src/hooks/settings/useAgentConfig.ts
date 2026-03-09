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
    onDismissError: resetError,
    onSave: () => void saveSettings(),
  };
}
