import { beforeEach, describe, expect, it } from 'vitest';
import { useSidebarStore } from './sidebarStore';

describe('sidebarStore', () => {
  beforeEach(() => {
    useSidebarStore.setState({ isCollapsed: false, activeSection: 'kanban', projectsExpanded: true });
  });

  it('toggles collapse and sets collapse', () => {
    useSidebarStore.getState().toggleCollapse();
    expect(useSidebarStore.getState().isCollapsed).toBe(true);
    useSidebarStore.getState().setCollapsed(false);
    expect(useSidebarStore.getState().isCollapsed).toBe(false);
  });

  it('sets active section and project expansion', () => {
    useSidebarStore.getState().setActiveSection('settings');
    useSidebarStore.getState().setProjectsExpanded(false);
    expect(useSidebarStore.getState().activeSection).toBe('settings');
    expect(useSidebarStore.getState().projectsExpanded).toBe(false);
  });
});
