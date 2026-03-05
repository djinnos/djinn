import { useEffect } from "react";
import { useSettingsStore } from "@/stores/settingsStore";

export function useAgentConfig() {
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
    loadSettings();
    loadProviderModels();
  }, [loadSettings, loadProviderModels]);

  useEffect(() => {
    if (hasUnsavedChanges && !isSaving) {
      const timeoutId = setTimeout(() => {
        saveSettings();
      }, 1000);

      return () => clearTimeout(timeoutId);
    }
  }, [hasUnsavedChanges, isSaving, saveSettings]);

  return {
    modelPriorities,
    sessionLimits,
    availableModels,
    isLoading,
    isSaving,
    error,
    hasUnsavedChanges,
    onAddModel: addModelToRole,
    onRemoveModel: removeModelFromRole,
    onReorderModels: reorderModelsInRole,
    onUpdateSessionLimit: updateSessionLimit,
    onDismissError: resetError,
  };
}
