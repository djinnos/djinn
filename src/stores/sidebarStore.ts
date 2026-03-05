import { create } from 'zustand';
import { persist } from 'zustand/middleware';

export interface SidebarState {
  isCollapsed: boolean;
  activeSection: 'kanban' | 'roadmap' | 'settings';
}

export interface SidebarActions {
  toggleCollapse: () => void;
  setCollapsed: (collapsed: boolean) => void;
  setActiveSection: (section: 'kanban' | 'roadmap' | 'settings') => void;
}

const INITIAL_STATE: SidebarState = {
  isCollapsed: false,
  activeSection: 'kanban',
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

      setActiveSection: (section: 'kanban' | 'roadmap' | 'settings') => {
        set({ activeSection: section });
      },
    }),
    {
      name: 'djinnos-sidebar-storage',
      partialize: (state) => ({
        isCollapsed: state.isCollapsed,
      }),
    }
  )
);
