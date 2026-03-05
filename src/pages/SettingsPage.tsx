import { useState } from 'react';
import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';
import { NavLink, Navigate, useParams } from 'react-router-dom';
import { Input } from '@/components/ui/input';
import { InlineError } from '@/components/InlineError';
import { EmptyState } from '@/components/EmptyState';
import { AgentConfig } from '@/components/AgentConfig';
import { useProviders } from '@/hooks/settings/useProviders';
import { useProjects } from '@/hooks/settings/useProjects';
import { useAgentConfig } from '@/hooks/settings/useAgentConfig';

type SettingsCategory = 'providers' | 'projects' | 'general' | 'agents';

const categories: Array<{ key: SettingsCategory; label: string }> = [
  { key: 'providers', label: 'Providers' },
  { key: 'projects', label: 'Projects' },
  { key: 'general', label: 'General' },
  { key: 'agents', label: 'Agents' },
];

function ProvidersSettings() {
  const {
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
  } = useProviders();

  const [isAddOpen, setIsAddOpen] = useState(false);
  const [selectedProviderId, setSelectedProviderId] = useState('');
  const [apiKey, setApiKey] = useState('');
  const [customName, setCustomName] = useState('');
  const [customBaseUrl, setCustomBaseUrl] = useState('');

  const selectedProvider = providers.find((p) => p.id === selectedProviderId);

  const resetAddFlow = () => {
    setSelectedProviderId('');
    setApiKey('');
    setValidationStatus(null);
    setCustomName('');
    setCustomBaseUrl('');
  };

  const handleSave = async () => {
    const ok = await saveProvider(selectedProviderId, apiKey);
    if (!ok) return;
    setIsAddOpen(false);
    resetAddFlow();
  };

  const handleAddCustom = async () => {
    const ok = await addCustom(customName, customBaseUrl);
    if (!ok) return;
    setCustomName('');
    setCustomBaseUrl('');
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
                onBlur={() => void validateInline(selectedProviderId, apiKey)}
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
  const { projects, loading, error, busyProjectId, isAdding, loadProjects, handleAddProject, handleRemoveProject } = useProjects();

  if (loading) {
    return <div className="rounded-lg border border-border bg-card p-6">Loading projects...</div>;
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-lg font-semibold">Projects</h2>
          <p className="text-sm text-muted-foreground">Registered projects and defaults.</p>
        </div>
        <Button onClick={() => void handleAddProject()} disabled={isAdding}>
          {isAdding ? 'Adding...' : 'Add Project'}
        </Button>
      </div>

      {error && <InlineError message={error} onRetry={() => void loadProjects()} />}

      {projects.length === 0 ? (
        <div className="rounded-lg border border-border bg-card p-6 text-sm text-muted-foreground">
          No projects registered yet.
        </div>
      ) : (
        <div className="space-y-2">
          {projects.map((project, index) => (
            <div key={project.id} className="flex items-center justify-between rounded-lg border border-border bg-card p-4 gap-4">
              <div className="min-w-0">
                <div className="flex items-center gap-2">
                  <p className="font-medium">{project.name}</p>
                  {index === 0 && (
                    <span className="rounded bg-secondary px-2 py-0.5 text-xs text-secondary-foreground">Default</span>
                  )}
                </div>
                <p className="truncate text-xs text-muted-foreground">{project.path}</p>
              </div>
              <Button
                variant="destructive"
                size="sm"
                onClick={() => void handleRemoveProject(project)}
                disabled={busyProjectId === project.id}
              >
                {busyProjectId === project.id ? 'Removing...' : 'Remove'}
              </Button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function GeneralSettings({ onResetWizard }: { onResetWizard: () => void }) {
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
            <Button variant="outline" size="sm" onClick={onResetWizard}>
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

  const agentConfig = useAgentConfig();

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
          {category === 'general' && <GeneralSettings onResetWizard={agentConfig.handleResetWizard} />}
          {category === 'agents' && <AgentConfig {...agentConfig} />}
        </section>
      </div>
    </div>
  );
}
