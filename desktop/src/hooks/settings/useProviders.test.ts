import { renderHook, act, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const toastMocks = vi.hoisted(() => ({
  showToast: {
    success: vi.fn(),
    error: vi.fn(),
  },
}));

const settingsStoreMocks = vi.hoisted(() => ({
  store: {
    removeModelsByProvider: vi.fn(),
    saveSettings: vi.fn().mockResolvedValue(undefined),
    loadProviderModels: vi.fn().mockResolvedValue(undefined),
  },
}));

const serverMocks = vi.hoisted(() => ({
  fetchProviderCatalog: vi.fn(),
  fetchCredentialList: vi.fn(),
  invalidateProviderCatalogCache: vi.fn(),
  validateProviderApiKey: vi.fn(),
  saveProviderCredentials: vi.fn(),
  startProviderOAuth: vi.fn(),
  addCustomProvider: vi.fn(),
  removeProviderFull: vi.fn(),
}));

vi.mock('@/lib/toast', () => ({ showToast: toastMocks.showToast }));
vi.mock('@/stores/settingsStore', () => ({ useSettingsStore: () => settingsStoreMocks.store }));
vi.mock('@/api/server', () => ({
  fetchProviderCatalog: serverMocks.fetchProviderCatalog,
  fetchCredentialList: serverMocks.fetchCredentialList,
  invalidateProviderCatalogCache: serverMocks.invalidateProviderCatalogCache,
  validateProviderApiKey: serverMocks.validateProviderApiKey,
  saveProviderCredentials: serverMocks.saveProviderCredentials,
  startProviderOAuth: serverMocks.startProviderOAuth,
  addCustomProvider: serverMocks.addCustomProvider,
  removeProviderFull: serverMocks.removeProviderFull,
}));

import { useProviders } from './useProviders';

describe('useProviders', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    serverMocks.fetchProviderCatalog.mockResolvedValue([
      { id: 'openai', name: 'OpenAI' },
      { id: 'anthropic', name: 'Anthropic' },
    ]);
    serverMocks.fetchCredentialList.mockResolvedValue([{ provider_id: 'openai', configured: true }]);
    serverMocks.validateProviderApiKey.mockResolvedValue({ valid: true });
    serverMocks.startProviderOAuth.mockResolvedValue({ success: true });
    serverMocks.saveProviderCredentials.mockResolvedValue(undefined);
  });

  it('fetches catalog and builds configured/unconfigured providers', async () => {
    const { result } = renderHook(() => useProviders());

    await waitFor(() => expect(result.current.loading).toBe(false));

    expect(serverMocks.invalidateProviderCatalogCache).toHaveBeenCalled();
    expect(serverMocks.fetchProviderCatalog).toHaveBeenCalled();
    expect(serverMocks.fetchCredentialList).toHaveBeenCalled();
    expect(result.current.configuredProviders.map((p: { id: string }) => p.id)).toEqual(['openai']);
    expect(result.current.unconfiguredProviders.map((p: { id: string }) => p.id)).toEqual(['anthropic']);
  });

  it('saves credentials after validation', async () => {
    const { result } = renderHook(() => useProviders());
    await waitFor(() => expect(result.current.loading).toBe(false));

    let ok = false;
    await act(async () => {
      ok = await result.current.saveProvider('openai', '  secret-key  ');
    });

    expect(ok).toBe(true);
    expect(serverMocks.validateProviderApiKey).toHaveBeenCalledWith('openai', 'secret-key');
    expect(serverMocks.saveProviderCredentials).toHaveBeenCalledWith('openai', 'secret-key');
    expect(settingsStoreMocks.store.loadProviderModels).toHaveBeenCalled();
  });

  it('starts oauth connection flow', async () => {
    const { result } = renderHook(() => useProviders());
    await waitFor(() => expect(result.current.loading).toBe(false));

    let ok = false;
    await act(async () => {
      ok = await result.current.connectOAuth('anthropic');
    });

    expect(ok).toBe(true);
    expect(serverMocks.startProviderOAuth).toHaveBeenCalledWith('anthropic');
    expect(settingsStoreMocks.store.loadProviderModels).toHaveBeenCalled();
  });

  it('validates inline api key and sets status', async () => {
    const { result } = renderHook(() => useProviders());
    await waitFor(() => expect(result.current.loading).toBe(false));

    serverMocks.validateProviderApiKey.mockResolvedValueOnce({ valid: false, error: 'invalid' });

    await act(async () => {
      await result.current.validateInline('openai', ' key ');
    });

    expect(serverMocks.validateProviderApiKey).toHaveBeenCalledWith('openai', 'key');
    expect(result.current.validationStatus).toEqual({ type: 'error', message: 'invalid' });
  });
});
