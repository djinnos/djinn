import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/api/settings', async () => {
  const actual = await vi.importActual<typeof import('@/api/settings')>('@/api/settings');
  return {
    ...actual,
    fetchProviderModels: vi.fn(),
  };
});

vi.mock('@/api/userSettings', async () => {
  const actual = await vi.importActual<typeof import('@/api/userSettings')>(
    '@/api/userSettings',
  );
  return {
    ...actual,
    fetchUserSettings: vi.fn(),
    patchUserSettings: vi.fn(),
  };
});

import { useSettingsStore } from './settingsStore';
import { fetchProviderModels } from '@/api/settings';
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
      availableModels: [],
      isLoading: false,
      isSaving: false,
      error: null,
      hasUnsavedChanges: false,
    });
    vi.clearAllMocks();
  });

  it('loads per-user models with per-user caps', async () => {
    vi.mocked(fetchUserSettings).mockResolvedValue({
      autoApprovePrs: false,
      lanes: { plan: ['p1/m1'], implement: [], review: [] },
      maxSessions: { 'p1/m1': 3 },
    });
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
    st.removeModel(0);
    expect(useSettingsStore.getState().models).toHaveLength(0);
  });

  it('loads provider models and saves the per-user model list + caps', async () => {
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
    vi.mocked(patchUserSettings).mockResolvedValue({
      autoApprovePrs: false,
      lanes: { plan: ['p1/m1'], implement: ['p1/m1'], review: ['p1/m1'] },
      maxSessions: { 'p1/m1': 1 },
    });
    await useSettingsStore.getState().saveSettings();
    // The flat onboarding editor seeds the same list into all three lanes.
    expect(patchUserSettings).toHaveBeenCalledWith({
      lanes: { plan: ['p1/m1'], implement: ['p1/m1'], review: ['p1/m1'] },
      maxSessions: { 'p1/m1': 1 },
    });
  });

  it('persists per-user caps when a cap is edited', async () => {
    useSettingsStore.getState().addModel({ model: 'm1', provider: 'p1' });
    useSettingsStore.getState().updateMaxSessions(0, 4);
    vi.mocked(patchUserSettings).mockResolvedValue({
      autoApprovePrs: false,
      lanes: { plan: ['p1/m1'], implement: ['p1/m1'], review: ['p1/m1'] },
      maxSessions: { 'p1/m1': 4 },
    });
    await useSettingsStore.getState().saveSettings();
    expect(patchUserSettings).toHaveBeenCalledWith({
      lanes: { plan: ['p1/m1'], implement: ['p1/m1'], review: ['p1/m1'] },
      maxSessions: { 'p1/m1': 4 },
    });
  });
});
