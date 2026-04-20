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
  removeProviderFull: vi.fn(),
}));

vi.mock('@/lib/toast', () => ({ showToast: toastMocks.showToast }));
vi.mock('@/stores/settingsStore', () => ({ useSettingsStore: () => settingsStoreMocks.store }));
vi.mock('@/api/server', () => ({
  fetchProviderCatalog: serverMocks.fetchProviderCatalog,
  fetchCredentialList: serverMocks.fetchCredentialList,
  invalidateProviderCatalogCache: serverMocks.invalidateProviderCatalogCache,
  removeProviderFull: serverMocks.removeProviderFull,
}));

import { useProviders } from './useProviders';

describe('useProviders', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    serverMocks.fetchProviderCatalog.mockResolvedValue([
      { id: 'anthropic', name: 'Anthropic' },
      { id: 'chatgpt_codex', name: 'ChatGPT / Codex' },
    ]);
    serverMocks.fetchCredentialList.mockResolvedValue([
      { provider_id: 'anthropic', configured: true, valid: true },
      { provider_id: 'chatgpt_codex', configured: true, valid: true },
    ]);
    serverMocks.removeProviderFull.mockResolvedValue(undefined);
  });

  it('fetches catalog and builds configured list', async () => {
    const { result } = renderHook(() => useProviders());

    await waitFor(() => expect(result.current.loading).toBe(false));

    expect(serverMocks.invalidateProviderCatalogCache).toHaveBeenCalled();
    expect(serverMocks.fetchProviderCatalog).toHaveBeenCalled();
    expect(serverMocks.fetchCredentialList).toHaveBeenCalled();
    expect(result.current.configuredProviders.map((p: { id: string }) => p.id)).toEqual([
      'anthropic',
      'chatgpt_codex',
    ]);
  });

  it('identifies self-serve providers', async () => {
    const { result } = renderHook(() => useProviders());
    await waitFor(() => expect(result.current.loading).toBe(false));

    expect(result.current.isSelfServeProvider('chatgpt_codex')).toBe(true);
    expect(result.current.isSelfServeProvider('anthropic')).toBe(false);
  });

  it('removes self-serve providers via provider_remove', async () => {
    const { result } = renderHook(() => useProviders());
    await waitFor(() => expect(result.current.loading).toBe(false));

    await act(async () => {
      await result.current.removeProvider('chatgpt_codex');
    });

    expect(serverMocks.removeProviderFull).toHaveBeenCalledWith('chatgpt_codex');
    expect(settingsStoreMocks.store.loadProviderModels).toHaveBeenCalled();
    expect(toastMocks.showToast.success).toHaveBeenCalledWith('Provider removed');
  });

  it('refuses to remove deployment-provisioned providers', async () => {
    const { result } = renderHook(() => useProviders());
    await waitFor(() => expect(result.current.loading).toBe(false));

    await act(async () => {
      await result.current.removeProvider('anthropic');
    });

    expect(serverMocks.removeProviderFull).not.toHaveBeenCalled();
    expect(toastMocks.showToast.error).toHaveBeenCalledWith(
      'Provisioned via deployment',
      expect.objectContaining({ description: expect.stringContaining('Helm') }),
    );
  });
});
