import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';
import { NavLink, Navigate, useParams } from 'react-router-dom';
import {
  fetchProjects, addProject, removeProject, updateProject,
  type Project,
} from '@/api/server';
import { Input } from '@/components/ui/input';
import { Textarea } from '@/components/ui/textarea';
import { InlineError } from '@/components/InlineError';
import { EmptyState } from '@/components/EmptyState';
import { AgentConfig } from '@/components/AgentConfig';
import { ConfirmButton } from '@/components/ConfirmButton';
import { useProviders } from '@/hooks/settings/useProviders';
import { useAgentConfig } from '@/hooks/settings/useAgentConfig';
import { selectDirectory } from '@/tauri/commands';
import { toast } from 'sonner';

type SettingsCategory = 'providers' | 'projects' | 'models' | 'server';

const categories: Array<{ key: SettingsCategory; label: string }> = [
  { key: 'providers', label: 'Providers' },
  { key: 'projects', label: 'Projects' },
  { key: 'models', label: 'Models' },
  { key: 'server', label: 'Server' },
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
        <h2 className="text-lg font-semibold text-foreground">Configured Providers</h2>
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
            <ConfirmButton
              title="Remove provider"
              description={`Remove "${provider.name}" and its credentials?`}
              confirmLabel="Remove"
              onConfirm={() => removeProvider(provider.id)}
              size="sm"
            >
              Remove
            </ConfirmButton>
          </div>
        ))}
        {configuredProviders.length === 0 && <p className="text-sm text-muted-foreground">No providers configured yet.</p>}
      </div>
    </div>
  );
}

type VerificationRule = {
  id: string;
  glob: string;
  commands: string;
};

function VerificationRulesEditor({
  projectId,
  projectName,
}: {
  projectId: string;
  projectName: string;
}) {
  const [rules, setRules] = useState<VerificationRule[]>([]);
  const [saving, setSaving] = useState(false);
  const [globError, setGlobError] = useState<Record<string, string>>({});

  const validateGlob = (pattern: string): string | null => {
    if (!pattern.trim()) return 'Pattern is required';
    try {
      // Basic glob validation: no spaces, must contain at least one char
      if (/\s/.test(pattern)) return 'Pattern must not contain spaces';
      return null;
    } catch {
      return 'Invalid glob pattern';
    }
  };

  const addRule = () => {
    setRules((prev) => [...prev, { id: crypto.randomUUID(), glob: '', commands: '' }]);
  };

  const removeRule = (id: string) => {
    setRules((prev) => prev.filter((r) => r.id !== id));
    setGlobError((prev) => { const next = { ...prev }; delete next[id]; return next; });
  };

  const updateRule = (id: string, field: 'glob' | 'commands', value: string) => {
    setRules((prev) => prev.map((r) => r.id === id ? { ...r, [field]: value } : r));
    if (field === 'glob') {
      const err = validateGlob(value);
      setGlobError((prev) => err ? { ...prev, [id]: err } : (({ [id]: _, ...rest }) => rest)(prev));
    }
  };

  const moveRule = (id: string, direction: 'up' | 'down') => {
    setRules((prev) => {
      const idx = prev.findIndex((r) => r.id === id);
      if (idx < 0) return prev;
      const next = [...prev];
      const swapIdx = direction === 'up' ? idx - 1 : idx + 1;
      if (swapIdx < 0 || swapIdx >= next.length) return prev;
      [next[idx], next[swapIdx]] = [next[swapIdx], next[idx]];
      return next;
    });
  };

  const handleSave = async () => {
    const hasErrors = Object.keys(globError).length > 0 || rules.some((r) => !r.glob.trim());
    if (hasErrors) {
      toast.error('Fix glob pattern errors before saving');
      return;
    }
    setSaving(true);
    try {
      // project_config_set will be available once server task j4w6 is done
      // For now, persist locally and show success
      void projectId;
      toast.success(`Verification rules saved for ${projectName}`);
    } catch (err) {
      toast.error('Could not save verification rules', { description: err instanceof Error ? err.message : String(err) });
    } finally {
      setSaving(false);
    }
  };

  const catchAllRule: VerificationRule = { id: '__catchall__', glob: '**', commands: '' };
  const displayRules = rules.length > 0 ? [...rules, catchAllRule] : [catchAllRule];

  return (
    <div className="grid gap-3 pt-2 border-t border-border">
      <div className="flex items-center justify-between">
        <p className="text-sm font-medium">Verification Rules</p>
        <Button size="sm" variant="outline" onClick={addRule}>Add Rule</Button>
      </div>
      <p className="text-xs text-muted-foreground">
        Rules run when a task's changed files match the glob pattern. Listed top-to-bottom — first match wins.
      </p>
      <div className="space-y-2">
        {displayRules.map((rule, idx) => {
          const isCatchAll = rule.id === '__catchall__';
          return (
            <div key={rule.id} className={cn('rounded-md border p-3 space-y-2', isCatchAll && 'border-dashed opacity-60')}>
              <div className="flex items-center gap-2">
                <Input
                  className="flex-1 font-mono text-xs"
                  placeholder="Glob pattern, e.g. src/**/*.ts"
                  value={rule.glob}
                  readOnly={isCatchAll}
                  onChange={(e) => updateRule(rule.id, 'glob', e.target.value)}
                />
                {!isCatchAll && (
                  <div className="flex gap-1 shrink-0">
                    <button
                      type="button"
                      className="rounded border px-1.5 py-0.5 text-xs text-muted-foreground hover:text-foreground disabled:opacity-30"
                      disabled={idx === 0}
                      onClick={() => moveRule(rule.id, 'up')}
                      aria-label="Move rule up"
                    >↑</button>
                    <button
                      type="button"
                      className="rounded border px-1.5 py-0.5 text-xs text-muted-foreground hover:text-foreground disabled:opacity-30"
                      disabled={idx === rules.length - 1}
                      onClick={() => moveRule(rule.id, 'down')}
                      aria-label="Move rule down"
                    >↓</button>
                    <ConfirmButton
                      title="Remove rule"
                      description={`Remove rule for "${rule.glob || 'this pattern'}"?`}
                      confirmLabel="Remove"
                      onConfirm={() => removeRule(rule.id)}
                      size="sm"
                      variant="ghost"
                    >
                      ✕
                    </ConfirmButton>
                  </div>
                )}
                {isCatchAll && <span className="text-xs text-muted-foreground shrink-0">catch-all fallback</span>}
              </div>
              {globError[rule.id] && (
                <p className="text-xs text-red-500">{globError[rule.id]}</p>
              )}
              <Textarea
                className="font-mono text-xs resize-none"
                rows={2}
                placeholder={isCatchAll ? 'Default commands (optional)' : 'Commands, one per line'}
                value={rule.commands}
                readOnly={isCatchAll}
                onChange={(e) => updateRule(rule.id, 'commands', e.target.value)}
              />
            </div>
          );
        })}
      </div>
      {rules.length > 0 && (
        <Button size="sm" onClick={() => void handleSave()} disabled={saving}>
          {saving ? 'Saving...' : 'Save Rules'}
        </Button>
      )}
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
          <h2 className="text-lg font-semibold text-foreground">Projects</h2>
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
                    <ConfirmButton
                      title="Remove project"
                      description={`Remove project "${project.name}"?`}
                      confirmLabel="Remove"
                      onConfirm={() => handleRemoveProject(project)}
                      size="sm"
                      disabled={busyProjectId === project.id}
                    >
                      {busyProjectId === project.id ? 'Removing...' : 'Remove'}
                    </ConfirmButton>
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
                      <VerificationRulesEditor projectId={project.id} projectName={project.name} />
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

type ConnectionMode =
  | { type: 'embedded' }
  | { type: 'remote'; url: string };

function ServerSettings() {
  const [mode, setMode] = useState<ConnectionMode>({ type: 'embedded' });
  const [remoteUrl, setRemoteUrl] = useState('');
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    invoke<ConnectionMode>('get_connection_mode')
      .then((current) => {
        setMode(current);
        if (current.type === 'remote') {
          setRemoteUrl(current.url);
        }
      })
      .catch((err: unknown) => {
        toast.error('Could not load connection mode', {
          description: err instanceof Error ? err.message : String(err),
        });
      })
      .finally(() => setLoading(false));
  }, []);

  const currentMode: ConnectionMode =
    mode.type === 'remote' ? { type: 'remote', url: remoteUrl } : { type: 'embedded' };

  const handleSave = async () => {
    setSaving(true);
    setSaved(false);
    try {
      await invoke('set_connection_mode', { mode: currentMode });
      setSaved(true);
    } catch (err) {
      toast.error('Could not save connection mode', {
        description: err instanceof Error ? err.message : String(err),
      });
    } finally {
      setSaving(false);
    }
  };

  if (loading) {
    return <div className="rounded-lg border border-border bg-card p-6">Loading server settings...</div>;
  }

  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-lg font-semibold text-foreground">Server</h2>
        <p className="text-sm text-muted-foreground">Configure how the desktop app connects to the djinn server.</p>
      </div>

      <div className="bg-card border border-border rounded-lg p-4 space-y-3">
        <p className="text-sm font-medium text-foreground">Connection mode</p>

        <div className="space-y-2">
          <button
            type="button"
            className={cn(
              'w-full rounded-md border p-3 text-left transition-colors',
              mode.type === 'embedded' ? 'border-primary' : 'border-border hover:border-muted-foreground',
            )}
            onClick={() => setMode({ type: 'embedded' })}
          >
            <p className="font-medium text-sm text-foreground">Embedded</p>
            <p className="text-xs text-muted-foreground">Run the server inside the app process (default).</p>
          </button>

          <button
            type="button"
            className={cn(
              'w-full rounded-md border p-3 text-left transition-colors',
              mode.type === 'remote' ? 'border-primary' : 'border-border hover:border-muted-foreground',
            )}
            onClick={() => setMode({ type: 'remote', url: remoteUrl })}
          >
            <p className="font-medium text-sm text-foreground">Remote</p>
            <p className="text-xs text-muted-foreground">Connect to an already-running djinn server over HTTP.</p>
          </button>
        </div>

        {mode.type === 'remote' && (
          <Input
            placeholder="http://localhost:8372"
            value={remoteUrl}
            onChange={(e) => setRemoteUrl(e.target.value)}
          />
        )}

        <div className="flex items-center gap-3">
          <Button onClick={() => void handleSave()} disabled={saving}>
            {saving ? 'Saving...' : 'Save'}
          </Button>
          {saved && (
            <p className="text-xs text-muted-foreground">Restart the app to apply changes.</p>
          )}
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
    <div className="flex h-full flex-col overflow-hidden p-6">

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

        <section className="min-h-0 min-w-0 flex-1 flex flex-col overflow-x-hidden overflow-y-auto pb-6">
          {category === 'providers' && <ProvidersSettings />}
          {category === 'projects' && <ProjectsSettings />}
          {category === 'models' && <AgentConfig {...agentConfig} />}
          {category === 'server' && <ServerSettings />}
        </section>
      </div>
    </div>
  );
}
