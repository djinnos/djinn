import { useCallback, useEffect, useMemo, useState } from 'react';
import { useSettingsStore } from '@/stores/settingsStore';
import {
  addCustomProvider,
  removeProviderFull,
  fetchCredentialList,
  fetchProviderCatalog,
  invalidateProviderCatalogCache,
  saveProviderCredentials,
  startProviderOAuth,
  validateProviderApiKey,
  type Provider,
  type ProviderCredential,
} from '@/api/server';
import { showToast } from '@/lib/toast';

export function useProviders() {
  const [providers, setProviders] = useState<Provider[]>([]);
  const [credentials, setCredentials] = useState<ProviderCredential[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [validationStatus, setValidationStatus] = useState<{ type: 'success' | 'error'; message: string } | null>(null);
  const [validating, setValidating] = useState(false);
  const [saving, setSaving] = useState(false);
  const [oauthInProgress, setOauthInProgress] = useState(false);

  const loadData = useCallback(async () => {
    setLoading(true);
    setLoadError(null);
    invalidateProviderCatalogCache();
    const [catalogResult, credentialResult] = await Promise.allSettled([fetchProviderCatalog(), fetchCredentialList()]);

    if (catalogResult.status === 'fulfilled') {
      setProviders(catalogResult.value);
    } else {
      const message = catalogResult.reason instanceof Error ? catalogResult.reason.message : 'Failed to load provider catalog';
      setLoadError(message);
      showToast.error('Failed to load providers', { description: message });
    }

    if (credentialResult.status === 'fulfilled') {
      setCredentials(credentialResult.value);
    } else if (catalogResult.status === 'fulfilled') {
      // Catalog loaded but credentials failed — show a non-blocking warning
      showToast.error('Could not load credential status', {
        description: credentialResult.reason instanceof Error ? credentialResult.reason.message : 'Unknown error',
      });
    }

    setLoading(false);
  }, []);

  useEffect(() => {
    void loadData();
  }, [loadData]);

  const credentialByProvider = useMemo(() => new Map(credentials.map((entry) => [entry.provider_id, entry])), [credentials]);
  const configuredProviders = useMemo(() => providers.filter((p) => credentialByProvider.get(p.id)?.configured), [providers, credentialByProvider]);
  const unconfiguredProviders = useMemo(() => providers.filter((p) => !credentialByProvider.get(p.id)?.configured), [providers, credentialByProvider]);

  const { removeModelsByProvider, saveSettings: saveAgentSettings, loadProviderModels } = useSettingsStore();

  const validateInline = useCallback(async (providerId: string, apiKey: string) => {
    if (!providerId || !apiKey.trim()) return;
    setValidating(true);
    try {
      const result = await validateProviderApiKey(providerId, apiKey.trim());
      if (result.valid) {
        setValidationStatus({ type: 'success', message: 'API key is valid' });
      } else {
        setValidationStatus({ type: 'error', message: result.error ?? 'Validation failed' });
      }
    } finally {
      setValidating(false);
    }
  }, []);

  const saveProvider = useCallback(async (providerId: string, apiKey: string) => {
    if (!providerId || !apiKey.trim()) return false;
    setSaving(true);
    try {
      const validation = await validateProviderApiKey(providerId, apiKey.trim());
      if (!validation.valid) {
        setValidationStatus({ type: 'error', message: validation.error ?? 'Validation failed' });
        return false;
      }
      await saveProviderCredentials(providerId, apiKey.trim());
      await loadData();
      await loadProviderModels();
      showToast.success('Provider added', { description: 'Credentials saved successfully.' });
      return true;
    } catch (error) {
      showToast.error('Could not save API key', { description: error instanceof Error ? error.message : 'Unknown error' });
      return false;
    } finally {
      setSaving(false);
    }
  }, [loadData, loadProviderModels]);

  const addCustom = useCallback(async (name: string, baseUrl: string) => {
    if (!name.trim()) return false;
    setSaving(true);
    try {
      await addCustomProvider({ name: name.trim(), base_url: baseUrl.trim() || undefined });
      await loadData();
      await loadProviderModels();
      showToast.success('Custom provider added');
      return true;
    } finally {
      setSaving(false);
    }
  }, [loadData, loadProviderModels]);

  const connectOAuth = useCallback(async (providerId: string) => {
    setOauthInProgress(true);
    try {
      const result = await startProviderOAuth(providerId);
      if (!result.success) {
        showToast.error('OAuth failed', { description: result.error ?? 'Unknown error' });
        return false;
      }
      await loadData();
      await loadProviderModels();
      showToast.success('Connected via OAuth');
      return true;
    } catch (error) {
      showToast.error('OAuth failed', { description: error instanceof Error ? error.message : 'Unknown error' });
      return false;
    } finally {
      setOauthInProgress(false);
    }
  }, [loadData, loadProviderModels]);

  const removeProvider = useCallback(async (providerId: string) => {
    try {
      await removeProviderFull(providerId);
      removeModelsByProvider(providerId);
      await saveAgentSettings();
      await loadData();
      await loadProviderModels();
      showToast.success('Provider removed');
    } catch (error) {
      showToast.error('Could not remove provider', { description: error instanceof Error ? error.message : 'Unknown error' });
    }
  }, [loadData, loadProviderModels, removeModelsByProvider, saveAgentSettings]);

  return {
    providers,
    configuredProviders,
    unconfiguredProviders,
    loading,
    loadError,
    validationStatus,
    validating,
    saving,
    oauthInProgress,
    setValidationStatus,
    loadData,
    validateInline,
    saveProvider,
    connectOAuth,
    addCustom,
    removeProvider,
  };
}
