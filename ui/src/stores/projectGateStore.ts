import { create } from 'zustand';
import { fetchProjects } from '@/api/server';

interface ProjectGateState {
  /** null = not yet checked */
  hasProject: boolean | null;
  refresh: () => Promise<void>;
}

export const useProjectGateStore = create<ProjectGateState>((set) => ({
  hasProject: null,

  refresh: async () => {
    try {
      const projects = await fetchProjects();
      set({ hasProject: projects.length > 0 });
    } catch {
      // On error leave the gate open so we don't block the user indefinitely
      set({ hasProject: true });
    }
  },
}));
