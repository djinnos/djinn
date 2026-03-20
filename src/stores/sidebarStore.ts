import { create } from 'zustand';
import { persist } from 'zustand/middleware';

export type ActiveView = 'kanban' | 'chat' | 'settings' | 'agents' | 'metrics';

export interface SidebarState {
  isCollapsed: boolean;
  activeSection: ActiveView;
  /** Whether the projects list in the sidebar is expanded. */
  projectsExpanded: boolean;
}

export interface SidebarActions {
  toggleCollapse: () => void;
  setCollapsed: (collapsed: boolean) => void;
  setActiveSection: (section: ActiveView) => void;
  setProjectsExpanded: (expanded: boolean) => void;
}

const INITIAL_STATE: SidebarState = {
  isCollapsed: false,
  activeSection: 'kanban',
  projectsExpanded: true,
};

export const useSidebarStore = create<SidebarState & SidebarActions>()(
  persist(
    (set) => ({
      ...INITIAL_STATE,

      toggleCollapse: () => {
        set((state) => ({ isCollapsed: !state.isCollapsed }));
      },

      setCollapsed: (collapsed: boolean) => {
        set({ isCollapsed: collapsed });
      },

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
        isCollapsed: state.isCollapsed,
        projectsExpanded: state.projectsExpanded,
      }),
    }
  )
);
