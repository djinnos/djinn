import { useEffect, useMemo, useState } from 'react';
import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';
import { useWizardStore } from '@/stores/wizardStore';
import { NavLink, Navigate, useParams } from 'react-router-dom';
import {
  fetchCredentialList,
  fetchProviderCatalog,
  saveProviderCredentials,
  validateProviderApiKey,
  addCustomProvider,
  type Provider,
  type ProviderCredential,
} from '@/api/server';
import { Input } from '@/components/ui/input';
import { InlineError } from '@/components/InlineError';
import { EmptyState } from '@/components/EmptyState';
import { showToast } from '@/lib/toast';
import { AgentConfig } from '@/components/AgentConfig';

type SettingsCategory = 'providers' | 'projects' | 'general' | 'agents';

const categories: Array<{ key: SettingsCategory; label: string }> = [
  { key: 'providers', label: 'Providers' },
  { key: 'projects', label: 'Projects' },
  { key: 'general', label: 'General' },
  { key: 'agents', label: 'Agents' },
];

function ProvidersSettings() {
  const [providers, setProviders] = useState<Provider[]>([]);
  const [credentials, setCredentials] = useState<ProviderCredential[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [isAddOpen, setIsAddOpen] = useState(false);
  const [selectedProviderId, setSelectedProviderId] = useState('');
  const [apiKey, setApiKey] = useState('');
  const [validationStatus, setValidationStatus] = useState<{ type: 'success' | 'error'; message: string } | null>(null);
  const [validating, setValidating] = useState(false);
  const [saving, setSaving] = useState(false);
  const [customName, setCustomName] = useState('');
  const [customBaseUrl, setCustomBaseUrl] = useState('');

  const loadData = async () => {
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
  };

  useEffect(() => {
    void loadData();
  }, []);

  const credentialByProvider = useMemo(() => new Map(credentials.map((entry) => [entry.provider_id, entry])), [credentials]);

  const configuredProviders = providers.filter((p) => credentialByProvider.get(p.id)?.configured);
  const unconfiguredProviders = providers.filter((p) => !credentialByProvider.get(p.id)?.configured);
  const selectedProvider = providers.find((p) => p.id === selectedProviderId);

  const resetAddFlow = () => {
    setSelectedProviderId('');
    setApiKey('');
    setValidationStatus(null);
    setCustomName('');
    setCustomBaseUrl('');
  };

  const validateInline = async () => {
    if (!selectedProviderId || !apiKey.trim()) return;
    setValidating(true);
    try {
      const result = await validateProviderApiKey(selectedProviderId, apiKey.trim());
      if (result.valid) {
        setValidationStatus({ type: 'success', message: 'API key is valid' });
      } else {
        setValidationStatus({ type: 'error', message: result.error ?? 'Validation failed' });
      }
    } finally {
      setValidating(false);
    }
  };

  const handleSave = async () => {
    if (!selectedProviderId || !apiKey.trim()) return;
    setSaving(true);
    try {
      const validation = await validateProviderApiKey(selectedProviderId, apiKey.trim());
      if (!validation.valid) {
        setValidationStatus({ type: 'error', message: validation.error ?? 'Validation failed' });
        return;
      }
      await saveProviderCredentials(selectedProviderId, apiKey.trim());
      await loadData();
      showToast.success('Provider added', { description: 'Credentials saved successfully.' });
      setIsAddOpen(false);
      resetAddFlow();
    } catch (error) {
      showToast.error('Could not save API key', { description: error instanceof Error ? error.message : 'Unknown error' });
    } finally {
      setSaving(false);
    }
  };

  const handleAddCustom = async () => {
    if (!customName.trim()) return;
    setSaving(true);
    try {
      await addCustomProvider({ name: customName.trim(), base_url: customBaseUrl.trim() || undefined });
      await loadData();
      showToast.success('Custom provider added');
      setCustomName('');
      setCustomBaseUrl('');
    } finally {
      setSaving(false);
    }
  };

  if (loading) {
    return <div className="rounded-lg border border-border bg-card p-6">Loading providers...</div>;
  }

  if (loadError) {
    return <InlineError message={loadError} onRetry={() => void loadData()} />;
  }

  if (providers.length === 0) {
    return (
      <EmptyState
        title="No providers found"
        message="Add a provider to start connecting your workspace tools."
        actionLabel="Reload providers"
        onAction={() => void loadData()}
        illustration={<div className="text-4xl">🔌</div>}
      />
    );
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-lg font-semibold">Configured Providers</h2>
        <Button onClick={() => setIsAddOpen((v) => !v)}>{isAddOpen ? 'Close' : 'Add Provider'}</Button>
      </div>

      {isAddOpen && (
        <div className="rounded-lg border border-border bg-card p-4 space-y-4">
          <h3 className="font-medium">Provider catalog</h3>
          <div className="space-y-2">
            {unconfiguredProviders.map((provider) => (
              <button
                key={provider.id}
                type="button"
                className={cn('w-full rounded-md border p-3 text-left', selectedProviderId === provider.id && 'border-primary')}
                onClick={() => {
                  setSelectedProviderId(provider.id);
                  setApiKey('');
                  setValidationStatus(null);
                }}
              >
                <p className="font-medium">{provider.name}</p>
                <p className="text-xs text-muted-foreground">{provider.description}</p>
              </button>
            ))}
          </div>

          {selectedProvider && (
            <div className="space-y-2">
              <Input
                type="password"
                placeholder={`Enter ${selectedProvider.name} API key`}
                value={apiKey}
                onChange={(e) => {
                  setApiKey(e.target.value);
                  setValidationStatus(null);
                }}
                onBlur={() => void validateInline()}
              />
              {validationStatus && <p className={cn('text-xs', validationStatus.type === 'success' ? 'text-green-500' : 'text-red-500')}>{validationStatus.message}</p>}
              <Button onClick={() => void handleSave()} disabled={saving || validating || !apiKey.trim()}>
                {saving ? 'Saving...' : validating ? 'Validating...' : 'Save Provider'}
              </Button>
            </div>
          )}

          <div className="border-t pt-4 space-y-2">
            <h4 className="text-sm font-medium">Add custom provider</h4>
            <Input placeholder="Provider name" value={customName} onChange={(e) => setCustomName(e.target.value)} />
            <Input placeholder="Base URL (optional)" value={customBaseUrl} onChange={(e) => setCustomBaseUrl(e.target.value)} />
            <Button variant="outline" onClick={() => void handleAddCustom()} disabled={saving || !customName.trim()}>
              Add Custom Provider
            </Button>
          </div>
        </div>
      )}

      <div className="space-y-2">
        {configuredProviders.map((provider) => (
          <div key={provider.id} className="rounded-lg border border-border bg-card p-4">
            <p className="font-medium">{provider.name}</p>
            <p className="text-xs text-muted-foreground">Configured</p>
          </div>
        ))}
        {configuredProviders.length === 0 && <p className="text-sm text-muted-foreground">No providers configured yet.</p>}
      </div>
    </div>
  );
}
function ProjectsSettings() {
  return (
    <div className="rounded-lg border border-border bg-card p-6">
      <h2 className="mb-2 text-lg font-semibold">Projects</h2>
      <p className="text-sm text-muted-foreground">
        Configure project-specific preferences and defaults.
      </p>
    </div>
  );
}

function GeneralSettings() {
  const { resetWizard } = useWizardStore();

  const handleResetWizard = () => {
    if (confirm('Are you sure you want to reset the wizard? This will show the setup wizard on next launch.')) {
      resetWizard();
    }
  };

  return (
    <div className="space-y-6">
      <div className="rounded-lg border border-border bg-card p-6">
        <h2 className="mb-4 text-lg font-semibold">General</h2>
        <div className="space-y-4">
          <div className="flex items-center justify-between">
            <div>
              <p className="font-medium">Theme</p>
              <p className="text-sm text-muted-foreground">Dark mode is enabled by default</p>
            </div>
            <span className="rounded bg-secondary px-2 py-1 text-xs">Dark</span>
          </div>
        </div>
      </div>

      <div className="rounded-lg border border-border bg-card p-6">
        <h2 className="mb-4 text-lg font-semibold">Setup</h2>
        <div className="space-y-4">
          <div className="flex items-center justify-between">
            <div>
              <p className="font-medium">Setup Wizard</p>
              <p className="text-sm text-muted-foreground">Reset the setup wizard to show on next launch</p>
            </div>
            <Button variant="outline" size="sm" onClick={handleResetWizard}>
              Reset Wizard
            </Button>
          </div>
        </div>
      </div>
    </div>
  );
}

export function SettingsPage() {
  const params = useParams<{ category?: string }>();
  const category = params.category as SettingsCategory | undefined;

  if (!category) {
    return <Navigate to="/settings/providers" replace />;
  }

  if (!categories.some((item) => item.key === category)) {
    return <Navigate to="/settings/providers" replace />;
  }

  return (
    <div className="flex h-full flex-col p-6">
      <div className="mb-6">
        <h1 className="text-2xl font-bold text-foreground">Settings</h1>
        <p className="mt-1 text-muted-foreground">Configure your workspace preferences</p>
      </div>

      <div className="flex flex-1 flex-col gap-6 md:flex-row">
        <aside className="md:w-56 md:shrink-0">
          <nav className="flex flex-row gap-2 overflow-x-auto md:flex-col md:overflow-visible">
            {categories.map((item) => (
              <NavLink
                key={item.key}
                to={`/settings/${item.key}`}
                className={({ isActive }) =>
                  cn(
                    'rounded-md px-3 py-2 text-sm transition-colors',
                    isActive
                      ? 'bg-primary text-primary-foreground'
                      : 'text-muted-foreground hover:bg-muted hover:text-foreground',
                  )
                }
              >
                {item.label}
              </NavLink>
            ))}
          </nav>
        </aside>

        <section className="min-w-0 flex-1">
          {category === 'providers' && <ProvidersSettings />}
          {category === 'projects' && <ProjectsSettings />}
          {category === 'general' && <GeneralSettings />}
          {category === 'agents' && <AgentConfig />}
        </section>
      </div>
    </div>
  );
}
