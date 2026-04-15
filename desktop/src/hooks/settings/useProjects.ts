import { useCallback, useEffect, useState } from 'react';
import { fetchProjects, removeProject, type Project } from '@/api/server';
import { showToast } from '@/lib/toast';

export function useProjects() {
  const [projects, setProjects] = useState<Project[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busyProjectId, setBusyProjectId] = useState<string | null>(null);
  const [isAdding, setIsAdding] = useState(false);

  const loadProjects = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await fetchProjects();
      setProjects(result);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load projects');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadProjects();
  }, [loadProjects]);

  // Migration 2: the server owns the filesystem — the Settings page no
  // longer has a local-directory picker. Add Project is now triggered from
  // the sidebar via <AddProjectFromGithubDialog>. This hook retains a stub
  // so callers can refresh the project list after an external add and can
  // display the "adding…" state during the refresh.
  const handleAddProject = useCallback(async () => {
    setIsAdding(true);
    setError(null);
    try {
      await loadProjects();
      showToast.info(
        'Add projects from the sidebar — the server clones the GitHub repo you pick.',
      );
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Failed to refresh projects';
      setError(message);
      showToast.error('Could not refresh projects', { description: message });
    } finally {
      setIsAdding(false);
    }
  }, [loadProjects]);

  const handleRemoveProject = useCallback(async (project: Project) => {
    setBusyProjectId(project.id);
    setError(null);
    try {
      await removeProject(project.id);
      await loadProjects();
      showToast.success('Project removed');
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Failed to remove project';
      setError(message);
      showToast.error('Could not remove project', { description: message });
    } finally {
      setBusyProjectId(null);
    }
  }, [loadProjects]);

  return { projects, loading, error, busyProjectId, isAdding, loadProjects, handleAddProject, handleRemoveProject };
}
