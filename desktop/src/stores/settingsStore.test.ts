import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/api/settings', () => ({
  fetchSettings: vi.fn(),
  saveSettings: vi.fn(),
  fetchProviderModels: vi.fn(),
}));

import { useSettingsStore } from './settingsStore';
import { fetchSettings, fetchProviderModels, saveSettings, type SettingsResponse } from '@/api/settings';
import type { ProviderModelsConnectedOutputSchema } from '@/api/generated/mcp-tools.gen';

describe('settingsStore', () => {
  beforeEach(() => {
    useSettingsStore.setState({
      models: [],
      availableModels: [],
      isLoading: false,
      isSaving: false,
      error: null,
      hasUnsavedChanges: false,
    });
    vi.clearAllMocks();
  });

  it('loads settings into unified models', async () => {
    vi.mocked(fetchSettings).mockResolvedValue({
      models: [{ model: 'm1', provider: 'p1', max_concurrent: 3 }],
    } satisfies SettingsResponse);
    await useSettingsStore.getState().loadSettings();
    expect(useSettingsStore.getState().models[0].model).toBe('m1');
    expect(useSettingsStore.getState().models[0].max_concurrent).toBe(3);
  });

  it('mutates model list actions', () => {
    const st = useSettingsStore.getState();
    st.addModel({ model: 'm1', provider: 'p1' });
    st.toggleRoleForModel(0, 'lead');
    st.updateMaxSessions(0, 5);
    st.reorderModels(0, 0);
    st.removeModelsByProvider('none');
    expect(useSettingsStore.getState().models).toHaveLength(1);
    expect(useSettingsStore.getState().models[0].max_concurrent).toBe(5);
    expect(useSettingsStore.getState().hasUnsavedChanges).toBe(true);
    st.removeModel(0);
    expect(useSettingsStore.getState().models).toHaveLength(0);
  });

  it('loads provider models and saves settings', async () => {
    const providerModels = [
      {
        id: 'p1/m1',
        name: 'm1',
        provider_id: 'p1',
        tool_call: true,
      },
    ] satisfies ProviderModelsConnectedOutputSchema.ProviderModelOutput[];
    vi.mocked(fetchProviderModels).mockResolvedValue(providerModels);
    await useSettingsStore.getState().loadProviderModels();
    expect(useSettingsStore.getState().availableModels).toHaveLength(1);

    useSettingsStore.getState().addModel({ model: 'm1', provider: 'p1' });
    vi.mocked(saveSettings).mockResolvedValue(undefined);
    await useSettingsStore.getState().saveSettings();
    expect(saveSettings).toHaveBeenCalled();
  });
});
