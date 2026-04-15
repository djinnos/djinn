import { useEffect, useMemo, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { Spinner } from '@/components/ui/spinner';
import { ScrollArea } from '@/components/ui/scroll-area';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  listGithubRepos,
  listGithubInstallations,
  addProjectFromGithub,
  type GithubRepoEntry,
  type Installation,
  type Project,
} from '@/api/server';
import { showToast } from '@/lib/toast';

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onAdded: (project: Project) => void;
}

export const INSTALLATIONS_QUERY_KEY = ['github', 'installations'] as const;
export const REPOS_QUERY_KEY = ['github', 'repos'] as const;

/**
 * Migration 2 replacement for the local-directory "Add Project" picker.
 *
 * Lists GitHub repos the Djinn App can access, filters client-side, and on
 * selection calls `project_add_from_github` so the server clones the repo
 * into its managed volume (`/root/.djinn/projects/{owner}/{repo}`).
 *
 * Supports multi-installation accounts: the user picks which installation
 * (personal / org) they want to browse. When the user has no installations
 * we show an empty state with a deep link to install the App.
 */
export function AddProjectFromGithubDialog({ open, onOpenChange, onAdded }: Props) {
  const [query, setQuery] = useState('');
  const [addingKey, setAddingKey] = useState<string | null>(null);
  const [selectedInstallationId, setSelectedInstallationId] = useState<number | null>(null);

  const installationsQuery = useQuery({
    queryKey: INSTALLATIONS_QUERY_KEY,
    queryFn: listGithubInstallations,
    enabled: open,
    staleTime: 30_000,
  });

  const installations: Installation[] = installationsQuery.data?.installations ?? [];
  const installUrl = installationsQuery.data?.installUrl ?? null;

  const reposQuery = useQuery({
    queryKey: REPOS_QUERY_KEY,
    queryFn: () => listGithubRepos(100),
    enabled: open && installations.length > 0,
    staleTime: 30_000,
  });

  const repos: GithubRepoEntry[] = reposQuery.data ?? [];

  // Default-select first installation once loaded (or reset if current selection vanishes).
  useEffect(() => {
    if (!open) return;
    if (installations.length === 0) {
      setSelectedInstallationId(null);
      return;
    }
    const stillValid = installations.some((i) => i.id === selectedInstallationId);
    if (!stillValid) {
      setSelectedInstallationId(installations[0].id);
    }
  }, [open, installations, selectedInstallationId]);

  const selectedInstallation = useMemo(
    () => installations.find((i) => i.id === selectedInstallationId) ?? null,
    [installations, selectedInstallationId],
  );

  const filtered = useMemo(() => {
    const scoped = selectedInstallationId === null
      ? repos
      : repos.filter((r) => r.installation_id === selectedInstallationId);
    const q = query.trim().toLowerCase();
    if (!q) return scoped;
    return scoped.filter((r) =>
      `${r.owner}/${r.repo}`.toLowerCase().includes(q) ||
      (r.description?.toLowerCase().includes(q) ?? false),
    );
  }, [repos, query, selectedInstallationId]);

  const handleAdd = async (entry: GithubRepoEntry) => {
    const key = `${entry.owner}/${entry.repo}`;
    setAddingKey(key);
    try {
      const project = await addProjectFromGithub({
        owner: entry.owner,
        repo: entry.repo,
        installation_id: entry.installation_id,
      });
      showToast.success(`Added ${key}`);
      onAdded(project);
      onOpenChange(false);
    } catch (err) {
      const msg = err instanceof Error ? err.message : 'Failed to add project';
      showToast.error('Could not add project', { description: msg });
    } finally {
      setAddingKey(null);
    }
  };

  const loading = installationsQuery.isLoading || (installations.length > 0 && reposQuery.isLoading);
  const loadError =
    installationsQuery.error instanceof Error
      ? installationsQuery.error.message
      : reposQuery.error instanceof Error
        ? reposQuery.error.message
        : null;

  const showEmptyInstallState = !loading && !loadError && installations.length === 0;
  const showRepoPicker = !loading && !loadError && installations.length > 0;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-xl">
        <DialogHeader>
          <DialogTitle>Add project from GitHub</DialogTitle>
          <DialogDescription>
            Pick a repository the Djinn GitHub App can access. The server will
            clone it into its managed storage.
          </DialogDescription>
        </DialogHeader>

        {loading && (
          <div className="flex items-center gap-2 py-6 text-sm text-muted-foreground">
            <Spinner className="h-4 w-4" />
            Loading repositories…
          </div>
        )}

        {loadError && (
          <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive">
            {loadError}
          </div>
        )}

        {showEmptyInstallState && (
          <div className="flex flex-col items-center gap-3 rounded-md border border-dashed px-6 py-8 text-center">
            <h3 className="text-sm font-semibold">
              Install Djinn on GitHub
            </h3>
            <p className="text-sm text-muted-foreground">
              Djinn needs access to your repositories before it can add a
              project. Install the GitHub App, then come back here.
            </p>
            <Button
              type="button"
              disabled={!installUrl}
              onClick={() => {
                if (installUrl) window.open(installUrl, '_blank', 'noopener,noreferrer');
              }}
            >
              Install Djinn on GitHub
            </Button>
            {!installUrl && (
              <p className="text-xs text-muted-foreground">
                The server has no public install URL configured.
              </p>
            )}
          </div>
        )}

        {showRepoPicker && (
          <>
            <div className="flex items-center gap-2">
              <Select
                value={selectedInstallationId !== null ? String(selectedInstallationId) : undefined}
                onValueChange={(value) => setSelectedInstallationId(Number(value))}
              >
                <SelectTrigger className="w-56">
                  <SelectValue placeholder="Select installation">
                    {selectedInstallation ? `${selectedInstallation.accountLogin}` : null}
                  </SelectValue>
                </SelectTrigger>
                <SelectContent>
                  {installations.map((installation) => (
                    <SelectItem key={installation.id} value={String(installation.id)}>
                      {installation.accountLogin}
                      <span className="ml-2 text-xs text-muted-foreground">
                        {installation.accountType}
                      </span>
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <Input
                placeholder="Search repositories..."
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                autoFocus
                className="flex-1"
              />
            </div>

            <ScrollArea className="h-80 rounded-md border">
              <ul className="divide-y">
                {filtered.length === 0 ? (
                  <li className="px-3 py-4 text-sm text-muted-foreground">
                    No repositories match.
                  </li>
                ) : (
                  filtered.map((r) => {
                    const key = `${r.owner}/${r.repo}`;
                    return (
                      <li
                        key={`${r.installation_id}:${key}`}
                        className="flex items-center gap-3 px-3 py-2 hover:bg-accent/40"
                      >
                        <div className="min-w-0 flex-1">
                          <div className="truncate text-sm font-medium">{key}</div>
                          {r.description && (
                            <div className="truncate text-xs text-muted-foreground">
                              {r.description}
                            </div>
                          )}
                        </div>
                        <Button
                          size="sm"
                          variant="secondary"
                          disabled={addingKey !== null}
                          onClick={() => void handleAdd(r)}
                        >
                          {addingKey === key ? <Spinner className="h-3 w-3" /> : 'Add'}
                        </Button>
                      </li>
                    );
                  })
                )}
              </ul>
            </ScrollArea>

            {installUrl && (
              <div className="flex justify-end pt-1">
                <a
                  href={installUrl}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="text-xs text-muted-foreground underline-offset-4 hover:text-foreground hover:underline"
                >
                  Install on another account →
                </a>
              </div>
            )}
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}
