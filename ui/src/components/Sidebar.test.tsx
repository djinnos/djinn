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
            { id: 'p-1', short_id: 'aaaa', title: 'Draft 1', status: 'draft', ac_total: 0, ac_met: 0, pending_reconcile: false, unresolved_feedback_count: 0, created_at: '', updated_at: '' },
            { id: 'p-2', short_id: 'bbbb', title: 'In Review 2', status: 'in_review', ac_total: 0, ac_met: 0, pending_reconcile: false, unresolved_feedback_count: 0, created_at: '', updated_at: '' },
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

  it('excludes terminal statuses (done, rejected, archived, superseded) from the badge count', async () => {
    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === 'proposal_list') {
        return {
          proposals: [
            // Active — counted
            { id: 'p-triage', short_id: 'tria', title: 'Triage 1', status: 'triage', ac_total: 0, ac_met: 0, pending_reconcile: false, unresolved_feedback_count: 0, created_at: '', updated_at: '' },
            { id: 'p-draft', short_id: 'draf', title: 'Draft 1', status: 'draft', ac_total: 0, ac_met: 0, pending_reconcile: false, unresolved_feedback_count: 0, created_at: '', updated_at: '' },
            { id: 'p-review', short_id: 'revw', title: 'Review 1', status: 'in_review', ac_total: 0, ac_met: 0, pending_reconcile: false, unresolved_feedback_count: 0, created_at: '', updated_at: '' },
            { id: 'p-approved', short_id: 'aprv', title: 'Approved 1', status: 'approved', ac_total: 0, ac_met: 0, pending_reconcile: false, unresolved_feedback_count: 0, created_at: '', updated_at: '' },
            { id: 'p-building', short_id: 'bldg', title: 'Building 1', status: 'building', ac_total: 0, ac_met: 0, pending_reconcile: false, unresolved_feedback_count: 0, created_at: '', updated_at: '' },
            // Terminal — NOT counted
            { id: 'p-done', short_id: 'done', title: 'Done 1', status: 'done', ac_total: 0, ac_met: 0, pending_reconcile: false, unresolved_feedback_count: 0, created_at: '', updated_at: '' },
            { id: 'p-rejected', short_id: 'rjct', title: 'Rejected 1', status: 'rejected', ac_total: 0, ac_met: 0, pending_reconcile: false, unresolved_feedback_count: 0, created_at: '', updated_at: '' },
            { id: 'p-archived', short_id: 'arch', title: 'Archived 1', status: 'archived', ac_total: 0, ac_met: 0, pending_reconcile: false, unresolved_feedback_count: 0, created_at: '', updated_at: '' },
            { id: 'p-superseded', short_id: 'supr', title: 'Superseded 1', status: 'superseded', ac_total: 0, ac_met: 0, pending_reconcile: false, unresolved_feedback_count: 0, created_at: '', updated_at: '' },
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

    // Five active statuses in the fixture, four terminal ones — badge must be 5.
    expect(await screen.findByLabelText('Proposals has 5 pending proposals')).toBeInTheDocument();
  });
});
