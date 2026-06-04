import { describe, it, expect, beforeEach, vi } from 'vitest';
import { render, screen, userEvent } from '@/test/test-utils';
import { Sidebar } from './Sidebar';
import { callMcpTool } from '@/api/mcpClient';
import { projectStore } from '@/stores/projectStore';
import { epicStore } from '@/stores/epicStore';
import { useSidebarStore } from '@/stores/sidebarStore';

vi.mock('@/api/mcpClient', () => ({
  callMcpTool: vi.fn(),
}));

vi.mock('@/lib/toast', () => ({
  showToast: { success: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

describe('Sidebar component', () => {
  beforeEach(() => {
    localStorage.clear();
    vi.mocked(callMcpTool).mockReset();
    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === 'proposal_list') {
        return { proposals: [] } as never;
      }

      return {} as never;
    });

    useSidebarStore.setState({
      activeSection: 'tasks',
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

  it('navigates between nav sections on click', async () => {
    const user = userEvent.setup();
    render(<Sidebar />);

    const chatButton = screen.getByRole('button', { name: /Chat/ });
    await user.click(chatButton);
    expect(useSidebarStore.getState().activeSection).toBe('chat');

    const tasksButton = screen.getByRole('button', { name: /Tasks/ });
    await user.click(tasksButton);
    expect(useSidebarStore.getState().activeSection).toBe('tasks');
  });

  it('renders sidebar with fixed width and all nav items', () => {
    const { container } = render(<Sidebar />);

    const sidebar = container.querySelector('aside');
    expect(sidebar?.className).toContain('w-64');

    expect(screen.getByRole('button', { name: /Chat/ })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /Tasks/ })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /Agents/ })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /Memory/ })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /Repositories/ })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /Settings/ })).toBeInTheDocument();
  });

  it('shows a Proposals badge matching the active proposal count', async () => {
    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === 'proposal_list') {
        return {
          proposals: [
            { id: 'p-1', short_id: 'aaaa', title: 'Draft 1', status: 'draft', acceptance_criteria: [], body: '', created_at: '', updated_at: '' },
            { id: 'p-2', short_id: 'bbbb', title: 'In Review 2', status: 'in_review', acceptance_criteria: [], body: '', created_at: '', updated_at: '' },
          ],
        } as never;
      }

      return {} as never;
    });

    render(<Sidebar />, {
      wrapperOptions: {
        routerProps: {
          initialEntries: ['/proposals'],
        },
      },
    });

    expect(await screen.findByLabelText('Proposals has 2 pending proposals')).toBeInTheDocument();
  });
});
