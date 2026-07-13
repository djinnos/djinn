import { useCallback, useEffect, useMemo, useState } from 'react';
import { useSettingsStore } from '@/stores/settingsStore';
import { sseStore } from '@/stores/sseStore';
import {
  removeProviderFull,
  fetchCredentialList,
  fetchProviderCatalog,
  invalidateProviderCatalogCache,
  type Provider,
  type ProviderCredential,
} from '@/api/server';
import { isSubscriptionProvider } from '@/api/subscriptionProviders';
import { showToast } from '@/lib/toast';

/**
 * Providers a user can self-serve (connect AND remove) from the UI: every
 * personal subscription (ChatGPT/Codex device-code OAuth + the BYO-key coding
 * plans — Kimi, MiniMax, Z.AI, Xiaomi, OpenCode, …). Removing one deletes only
 * the acting user's own credential server-side (`owner_user_id = current_user`),
 * so it never touches an org-provided key. Non-subscription providers are
 * deployment/operator-provisioned: a UI "remove" would only clear the vault row
 * and the next server restart re-bootstraps it from the operator env var, so we
 * keep those locked (managed).
 */
function isSelfServe(providerId: string): boolean {
  return providerId === 'chatgpt_codex' || isSubscriptionProvider(providerId);
}

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
    // eslint-disable-next-line react-hooks/set-state-in-effect -- mount trigger for an async catalog/credential load; the synchronous setLoading is the initial fetch transition, not derivable state.
    void loadData();
  }, [loadData]);

  // Live-refresh provider connection state when a credential changes server-side:
  // a token revoked by a 401 (flips a provider to "Disconnected — <reason>"), or
  // a (re)connect / removal. The catalog row is the F5-safe source of truth; this
  // just avoids a manual reload when the event arrives.
  useEffect(() => {
    const { subscribe } = sseStore.getState();
    const reload = () => {
      void loadData();
    };
    const unsubs = [
      subscribe('credential_revoked', reload),
      subscribe('credential_created', reload),
      subscribe('credential_updated', reload),
      subscribe('credential_deleted', reload),
    ];
    return () => unsubs.forEach((u) => u());
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
      if (!isSelfServe(providerId)) {
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
    (providerId: string) => isSelfServe(providerId),
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
