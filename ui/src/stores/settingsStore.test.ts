import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/api/settings', async () => {
  const actual = await vi.importActual<typeof import('@/api/settings')>('@/api/settings');
  return {
    ...actual,
    fetchProviderModels: vi.fn(),
    fetchGlobalMaxSessions: vi.fn(),
    saveGlobalMaxSessions: vi.fn(),
  };
});

vi.mock('@/api/userSettings', () => ({
  fetchUserSettings: vi.fn(),
  patchUserSettings: vi.fn(),
}));

import { useSettingsStore } from './settingsStore';
import {
  fetchProviderModels,
  fetchGlobalMaxSessions,
  saveGlobalMaxSessions,
} from '@/api/settings';
import { fetchUserSettings, patchUserSettings } from '@/api/userSettings';
import type { ProviderModelsConnectedOutputSchema } from '@/api/generated/mcp-tools.gen';

const PRICING = {
  cache_read_per_million: 0,
  cache_write_per_million: 0,
  input_per_million: 0,
  output_per_million: 0,
};

describe('settingsStore', () => {
  beforeEach(() => {
    useSettingsStore.setState({
      models: [],
      globalMaxSessions: {},
      availableModels: [],
      isLoading: false,
      isSaving: false,
      error: null,
      hasUnsavedChanges: false,
      hasUnsavedCapChanges: false,
    });
    vi.clearAllMocks();
  });

  it('loads per-user models and enriches with global caps', async () => {
    vi.mocked(fetchUserSettings).mockResolvedValue({
      autoApprovePrs: false,
      models: ['p1/m1'],
    });
    vi.mocked(fetchGlobalMaxSessions).mockResolvedValue({ 'p1/m1': 3 });
    await useSettingsStore.getState().loadSettings();
    expect(useSettingsStore.getState().models[0].model).toBe('m1');
    expect(useSettingsStore.getState().models[0].max_concurrent).toBe(3);
  });

  it('mutates model list actions', () => {
    const st = useSettingsStore.getState();
    st.addModel({ model: 'm1', provider: 'p1' });
    st.updateMaxSessions(0, 5);
    st.reorderModels(0, 0);
    st.removeModelsByProvider('none');
    expect(useSettingsStore.getState().models).toHaveLength(1);
    expect(useSettingsStore.getState().models[0].max_concurrent).toBe(5);
    expect(useSettingsStore.getState().hasUnsavedChanges).toBe(true);
    expect(useSettingsStore.getState().hasUnsavedCapChanges).toBe(true);
    st.removeModel(0);
    expect(useSettingsStore.getState().models).toHaveLength(0);
  });

  it('loads provider models and saves the per-user model list', async () => {
    const providerModels = [
      {
        id: 'p1/m1',
        name: 'm1',
        provider_id: 'p1',
        tool_call: true,
        attachment: false,
        context_window: 0,
        output_limit: 0,
        reasoning: false,
        pricing: PRICING,
      },
    ] satisfies ProviderModelsConnectedOutputSchema.ProviderModelOutput[];
    vi.mocked(fetchProviderModels).mockResolvedValue(providerModels);
    await useSettingsStore.getState().loadProviderModels();
    expect(useSettingsStore.getState().availableModels).toHaveLength(1);

    useSettingsStore.getState().addModel({ model: 'm1', provider: 'p1' });
    vi.mocked(patchUserSettings).mockResolvedValue({ autoApprovePrs: false, models: ['p1/m1'] });
    await useSettingsStore.getState().saveSettings();
    expect(patchUserSettings).toHaveBeenCalledWith({ models: ['p1/m1'] });
    // No cap edit happened, so global save is skipped.
    expect(saveGlobalMaxSessions).not.toHaveBeenCalled();
  });

  it('persists global caps only when an admin edits a cap', async () => {
    useSettingsStore.getState().addModel({ model: 'm1', provider: 'p1' });
    useSettingsStore.getState().updateMaxSessions(0, 4);
    vi.mocked(patchUserSettings).mockResolvedValue({ autoApprovePrs: false, models: ['p1/m1'] });
    vi.mocked(saveGlobalMaxSessions).mockResolvedValue(undefined);
    await useSettingsStore.getState().saveSettings();
    expect(saveGlobalMaxSessions).toHaveBeenCalledWith({ 'p1/m1': 4 });
  });
});
