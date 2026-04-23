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
      if (toolName === 'propose_adr_list') {
        return { items: [] } as never;
      }

      return {} as never;
    });

    useSidebarStore.setState({
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

  it('navigates between nav sections on click', async () => {
    const user = userEvent.setup();
    render(<Sidebar />);

    const chatButton = screen.getByRole('button', { name: /Chat/ });
    await user.click(chatButton);
    expect(useSidebarStore.getState().activeSection).toBe('chat');

    const kanbanButton = screen.getByRole('button', { name: /Kanban/ });
    await user.click(kanbanButton);
    expect(useSidebarStore.getState().activeSection).toBe('kanban');
  });

  it('renders sidebar with fixed width and all nav items', () => {
    const { container } = render(<Sidebar />);

    const sidebar = container.querySelector('aside');
    expect(sidebar?.className).toContain('w-64');

    expect(screen.getByRole('button', { name: /Chat/ })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /Kanban/ })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /Agents/ })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /Memory/ })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /Repositories/ })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /Settings/ })).toBeInTheDocument();
  });

  it('shows a Proposals badge matching the cross-project pending proposal count', async () => {
    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === 'propose_adr_list') {
        return {
          items: [
            { id: 'adr-1', title: 'Draft 1', path: '/tmp/adr-1.md', project_id: 'project-a' },
            { id: 'adr-2', title: 'Draft 2', path: '/tmp/adr-2.md', project_id: 'project-b' },
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
