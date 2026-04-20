import { useCallback, useEffect, useMemo, useState } from 'react';
import { useSettingsStore } from '@/stores/settingsStore';
import {
  removeProviderFull,
  fetchCredentialList,
  fetchProviderCatalog,
  invalidateProviderCatalogCache,
  type Provider,
  type ProviderCredential,
} from '@/api/server';
import { showToast } from '@/lib/toast';

/**
 * Providers that users can self-serve via the UI (device-code OAuth). Every
 * other provider is expected to be deployment-provisioned; removing one from
 * the UI only clears the vault row but will be re-bootstrapped on the next
 * server restart from the operator-supplied env var.
 */
const SELF_SERVE_PROVIDER_IDS = new Set(['chatgpt_codex']);

export function useProviders() {
  const [providers, setProviders] = useState<Provider[]>([]);
  const [credentials, setCredentials] = useState<ProviderCredential[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);

  const loadData = useCallback(async () => {
    setLoading(true);
    setLoadError(null);
    invalidateProviderCatalogCache();
    const [catalogResult, credentialResult] = await Promise.allSettled([
      fetchProviderCatalog(),
      fetchCredentialList(),
    ]);

    if (catalogResult.status === 'fulfilled') {
      setProviders(catalogResult.value);
    } else {
      const message =
        catalogResult.reason instanceof Error
          ? catalogResult.reason.message
          : 'Failed to load provider catalog';
      setLoadError(message);
      showToast.error('Failed to load providers', { description: message });
    }

    if (credentialResult.status === 'fulfilled') {
      setCredentials(credentialResult.value);
    } else if (catalogResult.status === 'fulfilled') {
      showToast.error('Could not load credential status', {
        description:
          credentialResult.reason instanceof Error
            ? credentialResult.reason.message
            : 'Unknown error',
      });
    }

    setLoading(false);
  }, []);

  useEffect(() => {
    void loadData();
  }, [loadData]);

  const credentialByProvider = useMemo(
    () => new Map(credentials.map((entry) => [entry.provider_id, entry])),
    [credentials],
  );
  const configuredProviders = useMemo(
    () => providers.filter((p) => credentialByProvider.get(p.id)?.configured),
    [providers, credentialByProvider],
  );

  const { removeModelsByProvider, saveSettings: saveAgentSettings, loadProviderModels } =
    useSettingsStore();

  const removeProvider = useCallback(
    async (providerId: string) => {
      if (!SELF_SERVE_PROVIDER_IDS.has(providerId)) {
        showToast.error('Provisioned via deployment', {
          description:
            'API-key providers are configured through Helm values. Ask your operator to unset the key.',
        });
        return;
      }
      try {
        await removeProviderFull(providerId);
        removeModelsByProvider(providerId);
        await saveAgentSettings();
        await loadData();
        await loadProviderModels();
        showToast.success('Provider removed');
      } catch (error) {
        showToast.error('Could not remove provider', {
          description: error instanceof Error ? error.message : 'Unknown error',
        });
      }
    },
    [loadData, loadProviderModels, removeModelsByProvider, saveAgentSettings],
  );

  const isSelfServeProvider = useCallback(
    (providerId: string) => SELF_SERVE_PROVIDER_IDS.has(providerId),
    [],
  );

  return {
    providers,
    configuredProviders,
    loading,
    loadError,
    loadData,
    removeProvider,
    isSelfServeProvider,
  };
}
