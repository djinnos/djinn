import { useCallback, useEffect, useMemo, useState } from 'react';
import {
  addCustomProvider,
  fetchCredentialList,
  fetchProviderCatalog,
  saveProviderCredentials,
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

  const loadData = useCallback(async () => {
    setLoading(true);
    setLoadError(null);
    try {
      const [catalog, credentialList] = await Promise.all([fetchProviderCatalog(), fetchCredentialList()]);
      setProviders(catalog);
      setCredentials(credentialList);
    } catch (error) {
      const message = error instanceof Error ? error.message : 'Failed to load providers';
      setLoadError(message);
      showToast.error('Failed to load providers', { description: message });
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadData();
  }, [loadData]);

  const credentialByProvider = useMemo(() => new Map(credentials.map((entry) => [entry.provider_id, entry])), [credentials]);
  const configuredProviders = useMemo(() => providers.filter((p) => credentialByProvider.get(p.id)?.configured), [providers, credentialByProvider]);
  const unconfiguredProviders = useMemo(() => providers.filter((p) => !credentialByProvider.get(p.id)?.configured), [providers, credentialByProvider]);

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
      showToast.success('Provider added', { description: 'Credentials saved successfully.' });
      return true;
    } catch (error) {
      showToast.error('Could not save API key', { description: error instanceof Error ? error.message : 'Unknown error' });
      return false;
    } finally {
      setSaving(false);
    }
  }, [loadData]);

  const addCustom = useCallback(async (name: string, baseUrl: string) => {
    if (!name.trim()) return false;
    setSaving(true);
    try {
      await addCustomProvider({ name: name.trim(), base_url: baseUrl.trim() || undefined });
      await loadData();
      showToast.success('Custom provider added');
      return true;
    } finally {
      setSaving(false);
    }
  }, [loadData]);

  return {
    providers,
    configuredProviders,
    unconfiguredProviders,
    loading,
    loadError,
    validationStatus,
    validating,
    saving,
    setValidationStatus,
    loadData,
    validateInline,
    saveProvider,
    addCustom,
  };
}
