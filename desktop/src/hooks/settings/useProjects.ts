import { useCallback, useEffect, useState } from 'react';
import { addProject, fetchProjects, removeProject, type Project } from '@/api/server';
import { showToast } from '@/lib/toast';
import { selectDirectory, syncGithubTokens } from '@/electron/commands';

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

  const handleAddProject = useCallback(async () => {
    setIsAdding(true);
    setError(null);
    try {
      const path = await selectDirectory('Select Project Directory');
      if (!path) return;
      try {
        await addProject(path);
      } catch (err) {
        // If the server doesn't have GitHub tokens yet, re-sync and retry once.
        const msg = err instanceof Error ? err.message : '';
        if (msg.includes('Connect GitHub first')) {
          await syncGithubTokens();
          await addProject(path);
        } else {
          throw err;
        }
      }
      await loadProjects();
      showToast.success('Project added');
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Failed to add project';
      setError(message);
      showToast.error('Could not add project', { description: message });
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
