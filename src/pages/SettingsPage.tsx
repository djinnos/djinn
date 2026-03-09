import { useEffect, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';
import { NavLink, Navigate, useParams } from 'react-router-dom';
import { fetchProjects, addProject, removeProject, updateProject, type Project } from '@/api/server';
import { Input } from '@/components/ui/input';
import { InlineError } from '@/components/InlineError';
import { EmptyState } from '@/components/EmptyState';
import { AgentConfig } from '@/components/AgentConfig';
import { useProviders } from '@/hooks/settings/useProviders';
import { useAgentConfig } from '@/hooks/settings/useAgentConfig';
import { selectDirectory } from '@/tauri/commands';
import { toast } from 'sonner';
import { getCurrentWindow } from '@tauri-apps/api/window';

type SettingsCategory = 'providers' | 'projects' | 'agents';

const categories: Array<{ key: SettingsCategory; label: string }> = [
  { key: 'providers', label: 'Providers' },
  { key: 'projects', label: 'Projects' },
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
    validating: _validating,
    saving,
    oauthInProgress,
    setValidationStatus,
    loadData,
    validateInline,
    saveProvider,
    connectOAuth,
    addCustom,
    removeProvider,
  } = useProviders();

  const [isAddOpen, setIsAddOpen] = useState(false);
  const [selectedProviderId, setSelectedProviderId] = useState('');
  const [apiKey, setApiKey] = useState('');
  const [customName, setCustomName] = useState('');
  const [customBaseUrl, setCustomBaseUrl] = useState('');
  const [catalogFilter, setCatalogFilter] = useState('');
  const catalogFilterRef = useRef<HTMLInputElement>(null);

  const selectedProvider = providers.find((p) => p.id === selectedProviderId);

  const resetAddFlow = () => {
    setSelectedProviderId('');
    setApiKey('');
    setValidationStatus(null);
    setCustomName('');
    setCustomBaseUrl('');
    setCatalogFilter('');
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
    <div className="flex flex-col gap-4 flex-1 min-h-0">
      <div className="flex items-center justify-between shrink-0">
        <h2 className="text-lg font-semibold">Configured Providers</h2>
        <Button onClick={() => { setIsAddOpen((v) => !v); if (isAddOpen) resetAddFlow(); }}>{isAddOpen ? 'Close' : 'Add Provider'}</Button>
      </div>

      {isAddOpen && (
        <div className="rounded-lg border border-border bg-card p-4 flex flex-col gap-4 flex-1 min-h-0">
          <h3 className="font-medium">Provider catalog</h3>
          <Input
            ref={catalogFilterRef}
            autoFocus
            placeholder="Filter providers..."
            value={catalogFilter}
            onChange={(e) => setCatalogFilter(e.target.value)}
          />
          <div className="flex-1 min-h-0 overflow-y-auto space-y-2 p-0.5">
            {unconfiguredProviders
              .filter((p) => !catalogFilter || p.name.toLowerCase().includes(catalogFilter.toLowerCase()))
              .map((provider) => (
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
                {provider.description && <p className="text-xs text-muted-foreground">{provider.description}</p>}
              </button>
            ))}
          </div>

          {selectedProvider && (
            <div className="space-y-3">
              {selectedProvider.oauth_supported && (
                <Button
                  className="w-full"
                  onClick={() => void connectOAuth(selectedProviderId)}
                  disabled={oauthInProgress || saving}
                >
                  {oauthInProgress ? 'Waiting for browser...' : `Connect ${selectedProvider.name} with OAuth`}
                </Button>
              )}
              {selectedProvider.oauth_supported && selectedProvider.requires_api_key && (
                <div className="flex items-center gap-3 text-xs text-muted-foreground">
                  <div className="h-px flex-1 bg-border" />
                  <span>or enter an API key</span>
                  <div className="h-px flex-1 bg-border" />
                </div>
              )}
              {selectedProvider.requires_api_key && (
                <>
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
                  <Button
                    variant={selectedProvider.oauth_supported ? 'outline' : 'default'}
                    onMouseDown={(e) => e.preventDefault()}
                    onClick={() => void handleSave()}
                    disabled={saving || oauthInProgress || !apiKey.trim()}
                  >
                    {saving ? 'Saving...' : 'Save API Key'}
                  </Button>
                </>
              )}
            </div>
          )}

          <div className="border-t pt-4 space-y-2 shrink-0">
            <h4 className="text-sm font-medium">Add custom provider</h4>
            <Input placeholder="Provider name" value={customName} onChange={(e) => setCustomName(e.target.value)} />
            <Input placeholder="Base URL (optional)" value={customBaseUrl} onChange={(e) => setCustomBaseUrl(e.target.value)} />
            <Button variant="outline" onClick={() => void handleAddCustom()} disabled={saving || !customName.trim()}>
              Add Custom Provider
            </Button>
          </div>
        </div>
      )}

      <div className="space-y-2 shrink-0">
        {configuredProviders.map((provider) => (
          <div key={provider.id} className="flex items-center justify-between rounded-lg border border-border bg-card p-4">
            <div>
              <p className="font-medium">{provider.name}</p>
              <p className="text-xs text-muted-foreground">Configured</p>
            </div>
            <Button
              variant="destructive"
              size="sm"
              onClick={async () => { if (await confirm(`Remove provider "${provider.name}" and its credentials?`)) await removeProvider(provider.id); }}
            >
              Remove
            </Button>
          </div>
        ))}
        {configuredProviders.length === 0 && <p className="text-sm text-muted-foreground">No providers configured yet.</p>}
      </div>
    </div>
  );
}

function ProjectsSettings() {
  const [projects, setProjects] = useState<Project[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busyProjectId, setBusyProjectId] = useState<string | null>(null);
  const [isAdding, setIsAdding] = useState(false);
  const [expandedProjectId, setExpandedProjectId] = useState<string | null>(null);
  const [projectDrafts, setProjectDrafts] = useState<Record<string, { branch: string; auto_merge: boolean }>>({});
  const saveTimerRef = useRef<number | null>(null);

  const loadProjects = async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await fetchProjects();
      setProjects(result);
      setProjectDrafts(Object.fromEntries(result.map((project) => [project.id, { branch: project.branch ?? '', auto_merge: project.auto_merge ?? false }])));
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load projects');
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void loadProjects();
    return () => {
      if (saveTimerRef.current) {
        window.clearTimeout(saveTimerRef.current);
      }
    };
  }, []);

  const handleAddProject = async () => {
    setIsAdding(true);
    setError(null);
    try {
      const path = await selectDirectory('Select Project Directory');
      if (!path) return;
      await addProject(path);
      await loadProjects();
      toast.success('Project added');
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Failed to add project';
      setError(message);
      toast.error('Could not add project', { description: message });
    } finally {
      setIsAdding(false);
    }
  };

  const handleRemoveProject = async (project: Project) => {
    if (!confirm(`Remove project "${project.name}"?`)) return;

    setBusyProjectId(project.id);
    setError(null);
    try {
      await removeProject(project.id);
      await loadProjects();
      toast.success('Project removed');
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Failed to remove project';
      setError(message);
      toast.error('Could not remove project', { description: message });
    } finally {
      setBusyProjectId(null);
    }
  };

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
        <>
          <div className="space-y-2">
            {projects.map((project) => {
              const draft = projectDrafts[project.id] ?? { branch: '', auto_merge: false };
              const expanded = expandedProjectId === project.id;
              const triggerSave = (next: { branch: string; auto_merge: boolean }) => {
                setProjectDrafts((prev) => ({ ...prev, [project.id]: next }));
                if (saveTimerRef.current) window.clearTimeout(saveTimerRef.current);
                saveTimerRef.current = window.setTimeout(() => {
                  void updateProject(project.id, next).then(() => toast.success('Saved')).catch((err: unknown) => {
                    const message = err instanceof Error ? err.message : 'Failed to save project';
                    toast.error('Could not save project', { description: message });
                  });
                }, 500);
              };

              return (
                <div key={project.id} className="rounded-lg border border-border bg-card p-4 space-y-3">
                  <div className="flex items-center justify-between gap-4">
                    <button className="min-w-0 text-left" onClick={() => setExpandedProjectId(expanded ? null : project.id)}>
                      <div className="flex items-center gap-2">
                        <p className="font-medium">{project.name}</p>
                      </div>
                      <p className="truncate text-xs text-muted-foreground">{project.path}</p>
                    </button>
                    <Button
                      variant="destructive"
                      size="sm"
                      onClick={() => void handleRemoveProject(project)}
                      disabled={busyProjectId === project.id}
                    >
                      {busyProjectId === project.id ? 'Removing...' : 'Remove'}
                    </Button>
                  </div>
                  {expanded && (
                    <div className="grid gap-3 pt-2 border-t border-border">
                      <div className="space-y-1">
                        <p className="text-sm font-medium">Target branch</p>
                        <Input
                          value={draft.branch}
                          onChange={(e) => triggerSave({ ...draft, branch: e.target.value })}
                          placeholder="main"
                        />
                      </div>
                      <div className="flex items-center justify-between">
                        <p className="text-sm font-medium">Auto-merge</p>
                        <input
                          type="checkbox"
                          className="h-4 w-4"
                          checked={draft.auto_merge}
                          onChange={(e) => triggerSave({ ...draft, auto_merge: e.target.checked })}
                        />
                      </div>
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        </>
      )}
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
    <div className="flex h-full flex-col overflow-hidden p-6">
      <div
        className="mb-6 shrink-0 cursor-default select-none"
        onMouseDown={(e) => { if (e.button === 0 && e.target === e.currentTarget) void getCurrentWindow().startDragging(); }}
      >
        <h1
          className="text-2xl font-bold text-foreground"
          onMouseDown={(e) => { if (e.button === 0) void getCurrentWindow().startDragging(); }}
        >Settings</h1>
        <p
          className="mt-1 text-muted-foreground"
          onMouseDown={(e) => { if (e.button === 0) void getCurrentWindow().startDragging(); }}
        >Configure your workspace preferences</p>
      </div>

      <div className="flex min-h-0 flex-1 flex-col gap-6 md:flex-row">
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

        <section className="min-h-0 min-w-0 flex-1 flex flex-col overflow-y-auto pb-6">
          {category === 'providers' && <ProvidersSettings />}
          {category === 'projects' && <ProjectsSettings />}
          {category === 'agents' && <AgentConfig {...agentConfig} />}
        </section>
      </div>
    </div>
  );
}
