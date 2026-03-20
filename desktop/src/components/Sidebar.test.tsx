import { describe, it, expect, beforeEach, vi } from 'vitest';
import { render, screen, userEvent, within } from '@/test/test-utils';
import { Sidebar } from './Sidebar';
import { projectStore } from '@/stores/projectStore';
import { epicStore } from '@/stores/epicStore';
import { useSidebarStore } from '@/stores/sidebarStore';

vi.mock('@/hooks/useExecutionStatus', () => ({
  useExecutionStatus: () => ({ state: 'idle', refresh: vi.fn() }),
}));

vi.mock('@/hooks/useExecutionControl', () => ({
  useExecutionControl: () => ({ start: vi.fn(), pause: vi.fn(), resume: vi.fn() }),
}));

describe('Sidebar component', () => {
  beforeEach(() => {
    localStorage.clear();

    useSidebarStore.setState({
      isCollapsed: false,
      activeSection: 'kanban',
      projectsExpanded: true,
    });

    projectStore.setState({
      projects: [
        { id: 'project-a', name: 'Project Alpha', path: '/tmp/project-alpha' },
        { id: 'project-b', name: 'Project Beta', path: '/tmp/project-beta' },
      ],
      selectedProjectId: 'project-a',
      lastViewPerProject: {},
    });

    epicStore.getState().setEpics([
      {
        id: 'epic-1',
        title: 'Epic One',
        status: 'open',
        project_id: 'project-a',
        created_at: new Date().toISOString(),
        updated_at: new Date().toISOString(),
      } as never,
    ]);
  });

  it('renders project list from projectStore', () => {
    render(<Sidebar />);

    expect(screen.getByText('All Projects')).toBeInTheDocument();
    expect(screen.getByText('Project Alpha')).toBeInTheDocument();
    expect(screen.getByText('Project Beta')).toBeInTheDocument();
  });

  it('expands and collapses projects section on click', async () => {
    const user = userEvent.setup();
    render(<Sidebar />);

    const projectsToggle = screen.getByRole('button', { name: 'Projects' });

    await user.click(projectsToggle);
    expect(screen.queryByText('Project Alpha')).not.toBeInTheDocument();
    expect(screen.queryByText('Project Beta')).not.toBeInTheDocument();

    await user.click(projectsToggle);
    expect(screen.getByText('Project Alpha')).toBeInTheDocument();
    expect(screen.getByText('Project Beta')).toBeInTheDocument();
  });

  it('highlights active project based on route', () => {
    render(<Sidebar />, {
      wrapperOptions: {
        routerProps: {
          initialEntries: ['/projects/project-b/kanban'],
        },
      },
    });

    const projectBetaLabel = screen.getByText('Project Beta');
    const activeProjectButton = projectBetaLabel.closest('[role="button"]');
    expect(activeProjectButton).not.toBeNull();
    expect(activeProjectButton?.className).toContain('bg-white/[0.07]');
    expect(activeProjectButton?.className).toContain('font-medium');

    const projectAlphaLabel = screen.getByText('Project Alpha');
    const inactiveProjectButton = projectAlphaLabel.closest('[role="button"]');
    expect(inactiveProjectButton).not.toBeNull();
    expect(inactiveProjectButton?.className).toContain('text-muted-foreground');
  });

  it('sidebar collapse toggle changes layout', async () => {
    const user = userEvent.setup();
    const { container } = render(<Sidebar />);

    const sidebar = container.querySelector('aside');
    expect(sidebar?.className).toContain('w-64');

    await user.click(screen.getByRole('button', { name: 'Collapse' }));

    expect(sidebar?.className).toContain('w-14');
    expect(screen.getByRole('button', { name: 'Expand' })).toBeInTheDocument();
  });

  it('shows empty state for projects when projectStore is empty', async () => {
    projectStore.setState({
      projects: [],
      selectedProjectId: null,
      lastViewPerProject: {},
    });

    render(<Sidebar />);

    const nav = screen.getByRole('navigation');
    expect(within(nav).queryByText('Project Alpha')).not.toBeInTheDocument();
    expect(within(nav).queryByText('Project Beta')).not.toBeInTheDocument();
    expect(screen.getByText('All Projects')).toBeInTheDocument();
  });
});
