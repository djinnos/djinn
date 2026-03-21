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
    toggleRoleForModel,
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
    onToggleRole: toggleRoleForModel,
    memoryModel: null as string | null,
    // eslint-disable-next-line @typescript-eslint/no-unused-vars
    onSetMemoryModel: (_modelId: string) => { /* TODO: wire to settings */ },
    onDismissError: resetError,
    onSave: () => void saveSettings(),
  };
}
