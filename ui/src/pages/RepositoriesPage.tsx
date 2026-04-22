import { useCallback, useState } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useNavigate } from 'react-router-dom';
import { HugeiconsIcon } from '@hugeicons/react';
import {
  Delete02Icon,
  Folder02Icon,
  GithubIcon,
  LinkSquare02Icon,
  Loading02Icon,
  PlusSignIcon,
  Settings01Icon,
} from '@hugeicons/core-free-icons';

import { Button } from '@/components/ui/button';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from '@/components/ui/alert-dialog';
import { AddProjectFromGithubDialog } from '@/components/AddProjectFromGithubDialog';
import { ImageStatusBadge } from '@/components/ImageStatusBadge';
import { EmptyState } from '@/components/EmptyState';
import { useProjects, useSelectedProjectId } from '@/stores/useProjectStore';
import { projectStore } from '@/stores/projectStore';
import { useProjectRoute } from '@/hooks/useProjectRoute';
import {
  fetchProjectBranches,
  fetchProjects,
  removeProject,
  updateProject,
  type Project,
} from '@/api/server';
import { showToast } from '@/lib/toast';
import { cn } from '@/lib/utils';

function parseOwnerRepo(path: string | undefined | null): { owner: string; repo: string } | null {
  if (!path) return null;
  const segments = path.split('/').filter(Boolean);
  if (segments.length < 2) return null;
  const repo = segments[segments.length - 1];
  const owner = segments[segments.length - 2];
  if (!owner || !repo) return null;
  return { owner, repo };
}

function RemoveButton({
  project,
  onRemoved,
}: {
  project: Project;
  onRemoved: () => Promise<void> | void;
}) {
  const [open, setOpen] = useState(false);
  const [removing, setRemoving] = useState(false);

  const handleConfirm = async () => {
    setRemoving(true);
    try {
      await removeProject(project.id);
      showToast.success(`Removed ${project.name}`);
      await onRemoved();
      setOpen(false);
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Failed to remove repository';
      showToast.error('Could not remove repository', { description: message });
    } finally {
      setRemoving(false);
    }
  };

  return (
    <AlertDialog
      open={open}
      onOpenChange={(v) => {
        if (!removing) setOpen(v);
      }}
    >
      <AlertDialogTrigger
        render={
          <button
            type="button"
            className="flex h-7 w-7 items-center justify-center rounded-md border bg-transparent text-muted-foreground transition-colors hover:border-red-400/40 hover:bg-red-500/10 hover:text-red-400"
            title={`Remove ${project.name}`}
            onClick={(e) => e.stopPropagation()}
          />
        }
      >
        {removing ? (
          <HugeiconsIcon icon={Loading02Icon} size={14} className="animate-spin" />
        ) : (
          <HugeiconsIcon icon={Delete02Icon} size={14} />
        )}
      </AlertDialogTrigger>
      <AlertDialogContent size="sm">
        <AlertDialogHeader>
          <AlertDialogTitle>Remove {project.name}?</AlertDialogTitle>
          <AlertDialogDescription>
            This unregisters the repository from Djinn and removes its tasks, epics, and history.
            The clone on disk and the GitHub repository are not affected.
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel disabled={removing}>Cancel</AlertDialogCancel>
          <Button variant="destructive" disabled={removing} onClick={() => void handleConfirm()}>
            {removing ? (
              <>
                <HugeiconsIcon icon={Loading02Icon} size={16} className="animate-spin" />
                Removing...
              </>
            ) : (
              'Remove'
            )}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

function BranchPicker({ project }: { project: Project }) {
  const queryClient = useQueryClient();
  const current = project.branch ?? 'main';
  const [saving, setSaving] = useState(false);

  const branchesQuery = useQuery({
    queryKey: ['project', project.id, 'branches'],
    queryFn: () => fetchProjectBranches(project.id),
    staleTime: 30_000,
  });

  const branches = branchesQuery.data?.branches ?? [];
  // Always include the currently-configured branch so the trigger has a value
  // to display even before the list loads.
  const options = Array.from(new Set([current, ...branches]));

  const handleChange = async (next: string) => {
    if (next === current) return;
    setSaving(true);
    try {
      await updateProject(project.id, { branch: next });
      const refreshed = await fetchProjects();
      projectStore.getState().setProjects(refreshed);
      showToast.success(`Working branch set to ${next}`);
      void queryClient.invalidateQueries({ queryKey: ['project', project.id, 'branches'] });
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Failed to update branch';
      showToast.error('Could not change branch', { description: message });
    } finally {
      setSaving(false);
    }
  };

  return (
    <Select
      value={current}
      onValueChange={(v) => {
        if (typeof v === 'string') void handleChange(v);
      }}
      disabled={saving}
    >
      <SelectTrigger
        size="sm"
        className="w-[180px]"
        onClick={(e) => e.stopPropagation()}
      >
        {saving ? (
          <span className="flex items-center gap-2 text-muted-foreground">
            <HugeiconsIcon icon={Loading02Icon} size={12} className="animate-spin" />
            Updating...
          </span>
        ) : (
          <SelectValue placeholder="Branch" />
        )}
      </SelectTrigger>
      <SelectContent>
        {options.map((branch) => (
          <SelectItem key={branch} value={branch}>
            {branch}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}

function RepositoryRow({
  project,
  isSelected,
  onSelect,
  onRemoved,
}: {
  project: Project;
  isSelected: boolean;
  onSelect: () => void;
  onRemoved: () => Promise<void> | void;
}) {
  const coords = parseOwnerRepo(project.path);
  const githubUrl = coords ? `https://github.com/${coords.owner}/${coords.repo}` : null;
  const navigate = useNavigate();

  return (
    <tr
      onClick={onSelect}
      className={cn(
        'cursor-pointer border-b border-border/50 transition-colors hover:bg-white/[0.03]',
        isSelected && 'bg-white/[0.04]',
      )}
    >
      <td className="px-4 py-3">
        <div className="flex items-center gap-3">
          <HugeiconsIcon icon={Folder02Icon} className="h-4 w-4 shrink-0 text-muted-foreground" />
          <div className="min-w-0">
            <div className="truncate text-sm font-medium text-foreground">{project.name}</div>
            {coords && (
              <div className="truncate text-xs text-muted-foreground">
                {coords.owner}/{coords.repo}
              </div>
            )}
          </div>
        </div>
      </td>
      <td className="px-4 py-3" onClick={(e) => e.stopPropagation()}>
        <BranchPicker project={project} />
      </td>
      <td className="px-4 py-3">
        <ImageStatusBadge
          projectId={project.id}
          projectName={project.name}
        />
      </td>
      <td className="px-4 py-3">
        <div
          className="flex items-center justify-end gap-1.5"
          onClick={(e) => e.stopPropagation()}
        >
          <button
            type="button"
            onClick={() => navigate(`/projects/${project.id}/environment`)}
            className="flex h-7 w-7 items-center justify-center rounded-md border bg-transparent text-muted-foreground transition-colors hover:bg-white/5 hover:text-foreground"
            title="Edit environment config"
          >
            <HugeiconsIcon icon={Settings01Icon} size={14} />
          </button>
          {githubUrl && (
            <a
              href={githubUrl}
              target="_blank"
              rel="noopener noreferrer"
              className="flex h-7 w-7 items-center justify-center rounded-md border bg-transparent text-muted-foreground transition-colors hover:bg-white/5 hover:text-foreground"
              title="Open on GitHub"
            >
              <HugeiconsIcon icon={LinkSquare02Icon} size={14} />
            </a>
          )}
          <RemoveButton project={project} onRemoved={onRemoved} />
        </div>
      </td>
    </tr>
  );
}

export function RepositoriesPage() {
  const projects = useProjects();
  const selectedProjectId = useSelectedProjectId();
  const { navigateToProject } = useProjectRoute();
  const [dialogOpen, setDialogOpen] = useState(false);
  const [refreshing, setRefreshing] = useState(false);

  const refreshProjects = useCallback(async () => {
    setRefreshing(true);
    try {
      const refreshed = await fetchProjects();
      projectStore.getState().setProjects(refreshed);
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Failed to refresh projects';
      showToast.error('Project list refresh failed', { description: message });
    } finally {
      setRefreshing(false);
    }
  }, []);

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <header className="flex items-center justify-between border-b px-6 py-4">
        <div>
          <h1 className="text-lg font-semibold">Repositories</h1>
          <p className="text-sm text-muted-foreground">
            GitHub repositories cloned into this deployment.
          </p>
        </div>
        <Button onClick={() => setDialogOpen(true)} disabled={refreshing}>
          {refreshing ? (
            <HugeiconsIcon icon={Loading02Icon} className="h-4 w-4 animate-spin" />
          ) : (
            <HugeiconsIcon icon={PlusSignIcon} className="h-4 w-4" />
          )}
          Add repository
        </Button>
      </header>

      <div className="flex-1 overflow-y-auto px-6 py-6">
        <div className="mx-auto max-w-5xl">
          {projects.length === 0 ? (
            <EmptyState
              title="No repositories yet"
              message="Add a repository from GitHub to start managing it with Djinn."
              actionLabel="Add repository"
              onAction={() => setDialogOpen(true)}
              illustration={<HugeiconsIcon icon={GithubIcon} className="h-10 w-10" />}
            />
          ) : (
            <>
              <div className="overflow-hidden rounded-md border">
                <table className="w-full text-left">
                  <thead className="bg-white/[0.02]">
                    <tr className="border-b text-xs uppercase tracking-wide text-muted-foreground">
                      <th className="px-4 py-2.5 font-medium">Repository</th>
                      <th className="px-4 py-2.5 font-medium">Branch</th>
                      <th className="px-4 py-2.5 font-medium">Status</th>
                      <th className="px-4 py-2.5 text-right font-medium">Actions</th>
                    </tr>
                  </thead>
                  <tbody>
                    {projects.map((project) => (
                      <RepositoryRow
                        key={project.id}
                        project={project}
                        isSelected={selectedProjectId === project.id}
                        onSelect={() => navigateToProject(project.id)}
                        onRemoved={refreshProjects}
                      />
                    ))}
                  </tbody>
                </table>
              </div>
            </>
          )}
        </div>
      </div>

      <AddProjectFromGithubDialog
        open={dialogOpen}
        onOpenChange={setDialogOpen}
        onAdded={() => void refreshProjects()}
      />
    </div>
  );
}
