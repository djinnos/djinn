import { useMemo, useState } from 'react';
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
 * Single-org replacement for the local-directory "Add Project" picker.
 *
 * Lists GitHub repos the Djinn App can access and, on selection, calls
 * `project_add_from_github` so the server clones the repo into its managed
 * volume (`/root/.djinn/projects/{owner}/{repo}`).
 *
 * Phase 2 invariant: this deployment is pinned to exactly one GitHub org
 * (the `org_config` singleton), and the App can only be installed on that
 * org. There's at most one installation — no picker, no "install on
 * another account" link. If the user has uninstalled the App from the
 * bound org, show a reinstall CTA.
 */
export function AddProjectFromGithubDialog({ open, onOpenChange, onAdded }: Props) {
  const [query, setQuery] = useState('');
  const [addingKey, setAddingKey] = useState<string | null>(null);

  const installationsQuery = useQuery({
    queryKey: INSTALLATIONS_QUERY_KEY,
    queryFn: listGithubInstallations,
    enabled: open,
    staleTime: 30_000,
  });

  const installations: Installation[] = installationsQuery.data?.installations ?? [];
  const installUrl = installationsQuery.data?.installUrl ?? null;
  // Single-org mode: there is at most one installation, so we don't need
  // a picker. Take the first one for the header label.
  const installation = installations[0] ?? null;

  const reposQuery = useQuery({
    queryKey: REPOS_QUERY_KEY,
    queryFn: () => listGithubRepos(100),
    enabled: open && installations.length > 0,
    staleTime: 30_000,
  });

  const repos: GithubRepoEntry[] = reposQuery.data ?? [];

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return repos;
    return repos.filter((r) =>
      `${r.owner}/${r.repo}`.toLowerCase().includes(q) ||
      (r.description?.toLowerCase().includes(q) ?? false),
    );
  }, [repos, query]);

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
      {/* Arbitrary width value dodges any tailwind-merge grouping quirks with
          the base DialogContent's `sm:max-w-sm`. `overflow-hidden` belts the
          width — any flex child with `min-w-0` + `truncate` will obey it
          rather than blowing past the modal edge. */}
      <DialogContent className="w-full max-w-[min(calc(100vw-2rem),42rem)] overflow-hidden">
        <DialogHeader>
          <DialogTitle>Add project from GitHub</DialogTitle>
          <DialogDescription>
            {installation
              ? <>Pick a repository from <span className="font-medium text-foreground">{installation.accountLogin}</span>. The server will clone it into its managed storage.</>
              : 'Pick a repository the Djinn GitHub App can access. The server will clone it into its managed storage.'}
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
              Reinstall Djinn on GitHub
            </h3>
            <p className="text-sm text-muted-foreground">
              The Djinn GitHub App is no longer installed on the
              organization this deployment is bound to. Reinstall it to
              grant repository access, then come back here.
            </p>
            <Button
              type="button"
              disabled={!installUrl}
              onClick={() => {
                if (installUrl) window.open(installUrl, '_blank', 'noopener,noreferrer');
              }}
            >
              Reinstall Djinn on GitHub
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
            <Input
              placeholder="Search repositories..."
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              autoFocus
            />

            {/* `w-full` forces the ScrollArea root (base-ui defaults to
                shrink-to-fit) to match the dialog column. `max-h-[60vh]`
                bounds height. `min-w-0` on the <ul> lets the flex-children
                truncation inside <li> actually engage. */}
            <ScrollArea className="w-full max-h-[60vh] rounded-md border">
              <ul className="w-full min-w-0 divide-y">
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
                        className="flex w-full min-w-0 items-center gap-3 px-3 py-2 hover:bg-accent/40"
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
                          className="shrink-0"
                        >
                          {addingKey === key ? <Spinner className="h-3 w-3" /> : 'Add'}
                        </Button>
                      </li>
                    );
                  })
                )}
              </ul>
            </ScrollArea>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}
