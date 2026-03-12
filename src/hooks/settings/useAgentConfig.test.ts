import { renderHook } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useAgentConfig } from './useAgentConfig';

const resetWizard = vi.fn();
const settingsStore = {
  models: [{ id: 'm1' }],
  availableModels: [{ id: 'm2' }],
  isLoading: false,
  isSaving: false,
  error: null,
  hasUnsavedChanges: false,
  loadSettings: vi.fn(),
  loadProviderModels: vi.fn(),
  addModel: vi.fn(),
  removeModel: vi.fn(),
  reorderModels: vi.fn(),
  toggleRoleForModel: vi.fn(),
  updateMaxSessions: vi.fn(),
  saveSettings: vi.fn(),
  resetError: vi.fn(),
};

vi.mock('@/stores/wizardStore', () => ({ useWizardStore: () => ({ resetWizard }) }));
vi.mock('@/stores/settingsStore', () => ({ useSettingsStore: () => settingsStore }));

describe('useAgentConfig', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('loads settings/model catalog and maps handlers', () => {
    const { result } = renderHook(() => useAgentConfig());

    expect(settingsStore.loadSettings).toHaveBeenCalled();
    expect(settingsStore.loadProviderModels).toHaveBeenCalled();

    result.current.onToggleRole('model-a', 'reviewer');
    expect(settingsStore.toggleRoleForModel).toHaveBeenCalledWith('model-a', 'reviewer');

    result.current.onUpdateMaxSessions('model-a', 3);
    expect(settingsStore.updateMaxSessions).toHaveBeenCalledWith('model-a', 3);
  });

  it('resets wizard and saves', () => {
    const { result } = renderHook(() => useAgentConfig());

    result.current.handleResetWizard();
    expect(resetWizard).toHaveBeenCalled();

    result.current.onSave();
    expect(settingsStore.saveSettings).toHaveBeenCalled();
  });
});
