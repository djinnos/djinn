import { useEffect, useMemo, useState } from 'react';
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
  addProjectFromGithub,
  type GithubRepoEntry,
  type Project,
} from '@/api/server';
import { showToast } from '@/lib/toast';

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onAdded: (project: Project) => void;
}

/**
 * Migration 2 replacement for the local-directory "Add Project" picker.
 *
 * Lists GitHub repos the Djinn App can access, filters client-side, and on
 * selection calls `project_add_from_github` so the server clones the repo
 * into its managed volume (`/root/.djinn/projects/{owner}/{repo}`).
 */
export function AddProjectFromGithubDialog({ open, onOpenChange, onAdded }: Props) {
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [repos, setRepos] = useState<GithubRepoEntry[]>([]);
  const [query, setQuery] = useState('');
  const [addingKey, setAddingKey] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    setLoading(true);
    setError(null);
    listGithubRepos(100)
      .then((entries) => {
        if (!cancelled) setRepos(entries);
      })
      .catch((err) => {
        if (!cancelled) {
          setError(err instanceof Error ? err.message : 'Failed to load repositories');
        }
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [open]);

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

        <Input
          placeholder="Search repositories..."
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          autoFocus
        />

        {loading && (
          <div className="flex items-center gap-2 py-6 text-sm text-muted-foreground">
            <Spinner className="h-4 w-4" />
            Loading repositories…
          </div>
        )}

        {error && (
          <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive">
            {error}
          </div>
        )}

        {!loading && !error && (
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
                      key={key}
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
        )}
      </DialogContent>
    </Dialog>
  );
}
