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
  deleteProviderCredentials,
  type Provider,
  type ProviderCredential,
} from '@/api/server';
import { AlertCircleIcon, CheckCircle2Icon, CopyIcon, EyeIcon, EyeOffIcon, Loader2Icon, XCircleIcon } from 'lucide-react';
import { Input } from '@/components/ui/input';
import { AgentConfig } from '@/components/AgentConfig';

type SettingsCategory = 'providers' | 'projects' | 'general' | 'agents';
type ProviderStatus = 'connected' | 'error' | 'unconfigured';

const categories: Array<{ key: SettingsCategory; label: string }> = [
  { key: 'providers', label: 'Providers' },
  { key: 'projects', label: 'Projects' },
  { key: 'general', label: 'General' },
  { key: 'agents', label: 'Agents' },
];

function ProviderCard({
  provider,
  status,
  expanded,
  onClick,
}: {
  provider: Provider;
  status: ProviderStatus;
  expanded: boolean;
  onClick: () => void;
}) {
  const statusStyles = {
    connected: 'bg-green-500/15 text-green-500 border-green-500/25',
    error: 'bg-red-500/15 text-red-500 border-red-500/25',
    unconfigured: 'bg-muted text-muted-foreground border-border',
  } as const;

  const dotStyles = {
    connected: 'bg-green-500',
    error: 'bg-red-500',
    unconfigured: 'bg-gray-500',
  } as const;

  return (
    <button
      type="button"
      onClick={onClick}
      className="w-full rounded-lg border border-border bg-card p-4 text-left transition-colors hover:bg-muted/40"
    >
      <div className="flex items-center justify-between gap-3">
        <div className="flex items-center gap-3">
          <div className="flex h-10 w-10 items-center justify-center rounded-md bg-muted text-sm font-semibold uppercase">
            {provider.name.slice(0, 2)}
          </div>
          <div>
            <p className="font-medium">{provider.name}</p>
            <p className="text-sm text-muted-foreground">{provider.description}</p>
          </div>
        </div>
        <span className={cn('inline-flex items-center gap-2 rounded-full border px-2 py-1 text-xs', statusStyles[status])}>
          <span className={cn('h-2 w-2 rounded-full', dotStyles[status])} />
          {status}
        </span>
      </div>
      {expanded && <p className="mt-3 text-xs text-muted-foreground">Click again to collapse</p>}
    </button>
  );
}

function ProvidersSettings() {
  const [providers, setProviders] = useState<Provider[]>([]);
  const [credentials, setCredentials] = useState<ProviderCredential[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [expandedProvider, setExpandedProvider] = useState<string | null>(null);
  const [apiKey, setApiKey] = useState('');
  const [saving, setSaving] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [revealedProvider, setRevealedProvider] = useState<string | null>(null);
  const [validationStatus, setValidationStatus] = useState<{ type: 'success' | 'error'; message: string } | null>(null);
  const [testing, setTesting] = useState(false);

  useEffect(() => {
    const load = async () => {
      setLoading(true);
      setLoadError(null);
      try {
        const [catalog, credentialList] = await Promise.all([fetchProviderCatalog(), fetchCredentialList()]);
        setProviders(catalog);
        setCredentials(credentialList);
      } catch (error) {
        setLoadError(error instanceof Error ? error.message : 'Failed to load providers');
      } finally {
        setLoading(false);
      }
    };

    void load();
  }, []);

  useEffect(() => {
    if (!revealedProvider) return;
    const timer = window.setTimeout(() => setRevealedProvider(null), 10000);
    return () => window.clearTimeout(timer);
  }, [revealedProvider]);

  const credentialByProvider = useMemo(
    () => new Map(credentials.map((entry) => [entry.provider_id, entry])),
    [credentials],
  );

  const getStatus = (providerId: string): ProviderStatus => {
    const credential = credentialByProvider.get(providerId);
    if (!credential || !credential.configured) return 'unconfigured';
    return credential.valid ? 'connected' : 'error';
  };

  const maskApiKey = (key: string): string => {
    if (key.length <= 8) return key;
    return `${key.slice(0, 4)}...${key.slice(-4)}`;
  };

  const handleExpand = (providerId: string, expanded: boolean) => {
    const next = expanded ? null : providerId;
    setExpandedProvider(next);
    setApiKey('');
    setValidationStatus(null);
    setRevealedProvider(null);
  };

  const handleSave = async () => {
    if (!expandedProvider || !apiKey.trim()) return;
    setSaving(true);
    try {
      const validation = await validateProviderApiKey(expandedProvider, apiKey.trim());
      if (!validation.valid) {
        setValidationStatus({ type: 'error', message: validation.error ?? 'Validation failed' });
        return;
      }
      await saveProviderCredentials(expandedProvider, apiKey.trim());
      const credentialList = await fetchCredentialList();
      setCredentials(credentialList);
      setValidationStatus({ type: 'success', message: 'API key saved successfully' });
      setApiKey('');
    } finally {
      setSaving(false);
    }
  };

  const handleTestConnection = async (providerId: string) => {
    const keyToTest = apiKey.trim();
    const credential = credentialByProvider.get(providerId);
    if (!keyToTest && !credential?.configured) {
      setValidationStatus({ type: 'error', message: 'Enter an API key before testing connection' });
      return;
    }

    setTesting(true);
    try {
      const result = await validateProviderApiKey(providerId, keyToTest);
      if (result.valid) {
        setValidationStatus({ type: 'success', message: 'Connection successful' });
      } else {
        setValidationStatus({ type: 'error', message: result.error ?? 'Connection failed' });
      }
    } finally {
      setTesting(false);
    }
  };

  const handleDelete = async (providerId: string) => {
    if (!confirm('Delete this API key?')) return;
    setDeleting(true);
    try {
      await deleteProviderCredentials(providerId);
      const credentialList = await fetchCredentialList();
      setCredentials(credentialList);
      setValidationStatus({ type: 'success', message: 'API key deleted' });
      setRevealedProvider(null);
    } finally {
      setDeleting(false);
    }
  };

  if (loading) {
    return (
      <div className="rounded-lg border border-border bg-card p-6">
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          <Loader2Icon className="h-4 w-4 animate-spin" /> Loading providers...
        </div>
      </div>
    );
  }

  if (loadError) {
    return (
      <div className="rounded-lg border border-border bg-card p-6">
        <div className="flex items-start gap-2 text-destructive">
          <AlertCircleIcon className="mt-0.5 h-4 w-4" />
          <p className="text-sm">{loadError}</p>
        </div>
      </div>
    );
  }

  return (
    <div className="space-y-4">
      {providers.map((provider) => {
        const status = getStatus(provider.id);
        const expanded = expandedProvider === provider.id;
        const credential = credentialByProvider.get(provider.id);
        const isRevealed = revealedProvider === provider.id;
        const storedMasked = credential?.api_key_masked ? maskApiKey(credential.api_key_masked) : null;

        return (
          <div key={provider.id} className="space-y-2">
            <ProviderCard
              provider={provider}
              status={status}
              expanded={expanded}
              onClick={() => handleExpand(provider.id, expanded)}
            />

            {expanded && (
              <div className="rounded-lg border border-border bg-card p-4 space-y-3">
                <h3 className="text-sm font-medium">API Key Management</h3>

                {credential?.configured && storedMasked && (
                  <div className="rounded-md border border-border p-3">
                    <p className="text-xs text-muted-foreground mb-2">Stored key</p>
                    <div className="flex items-center gap-2">
                      <code className="text-sm">{isRevealed ? credential.api_key_masked : storedMasked}</code>
                      <Button variant="ghost" size="icon" onClick={() => setRevealedProvider(isRevealed ? null : provider.id)}>
                        {isRevealed ? <EyeOffIcon className="h-4 w-4" /> : <EyeIcon className="h-4 w-4" />}
                      </Button>
                      <Button
                        variant="ghost"
                        size="icon"
                        onClick={() => {
                          const value = credential.api_key_masked ?? '';
                          if (value) void navigator.clipboard.writeText(value);
                        }}
                      >
                        <CopyIcon className="h-4 w-4" />
                      </Button>
                    </div>
                  </div>
                )}

                <div className="flex gap-2">
                  <Input
                    type="password"
                    placeholder="Enter API key"
                    value={apiKey}
                    onChange={(e) => {
                      setApiKey(e.target.value);
                      setValidationStatus(null);
                    }}
                  />
                  <Button onClick={() => void handleSave()} disabled={saving || !apiKey.trim()}>
                    {saving ? <Loader2Icon className="h-4 w-4 animate-spin" /> : 'Save'}
                  </Button>
                </div>

                <div className="flex items-center gap-2">
                  <Button variant="outline" onClick={() => void handleTestConnection(provider.id)} disabled={testing}>
                    {testing ? <Loader2Icon className="h-4 w-4 animate-spin" /> : 'Test connection'}
                  </Button>
                  {credential?.configured && (
                    <Button variant="destructive" onClick={() => void handleDelete(provider.id)} disabled={deleting}>
                      Delete
                    </Button>
                  )}
                </div>

                {validationStatus && (
                  <div className={cn('text-xs', validationStatus.type === 'success' ? 'text-green-500' : 'text-red-500')}>
                    {validationStatus.message}
                  </div>
                )}

                <div className="flex items-center gap-2 text-xs text-muted-foreground">
                  {status === 'connected' && <CheckCircle2Icon className="h-4 w-4 text-green-500" />}
                  {status === 'error' && <XCircleIcon className="h-4 w-4 text-red-500" />}
                  {status === 'unconfigured' && <AlertCircleIcon className="h-4 w-4 text-gray-500" />}
                  Current status: {status}
                </div>
              </div>
            )}
          </div>
        );
      })}
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
