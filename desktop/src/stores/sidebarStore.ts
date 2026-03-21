import { create } from 'zustand';
import { persist } from 'zustand/middleware';

export type ActiveView = 'kanban' | 'chat' | 'settings' | 'agents' | 'metrics';

export interface SidebarState {
  activeSection: ActiveView;
  /** Whether the projects list in the sidebar is expanded. */
  projectsExpanded: boolean;
}

export interface SidebarActions {
  setActiveSection: (section: ActiveView) => void;
  setProjectsExpanded: (expanded: boolean) => void;
}

const INITIAL_STATE: SidebarState = {
  activeSection: 'kanban',
  projectsExpanded: true,
};

export const useSidebarStore = create<SidebarState & SidebarActions>()(
  persist(
    (set) => ({
      ...INITIAL_STATE,

      setActiveSection: (section: ActiveView) => {
        set({ activeSection: section });
      },

      setProjectsExpanded: (expanded: boolean) => {
        set({ projectsExpanded: expanded });
      },
    }),
    {
      name: 'djinnos-sidebar-storage',
      partialize: (state) => ({
        projectsExpanded: state.projectsExpanded,
      }),
    }
  )
);
