import { renderHook } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useAgentConfig } from './useAgentConfig';

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
  updateMaxSessions: vi.fn(),
  saveSettings: vi.fn(),
  resetError: vi.fn(),
};

vi.mock('@/stores/settingsStore', () => ({ useSettingsStore: () => settingsStore }));

describe('useAgentConfig', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('loads settings/model catalog and maps handlers', () => {
    const { result } = renderHook(() => useAgentConfig());

    expect(settingsStore.loadSettings).toHaveBeenCalled();
    expect(settingsStore.loadProviderModels).toHaveBeenCalled();

    result.current.onUpdateMaxSessions(0, 3);
    expect(settingsStore.updateMaxSessions).toHaveBeenCalledWith(0, 3);
  });

  it('saves settings', () => {
    const { result } = renderHook(() => useAgentConfig());

    result.current.onSave();
    expect(settingsStore.saveSettings).toHaveBeenCalled();
  });
});
