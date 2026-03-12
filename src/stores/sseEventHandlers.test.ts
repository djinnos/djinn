import { beforeEach, describe, expect, it, vi } from 'vitest';
import { initSSEEventHandlers } from './sseEventHandlers';
import { sseStore } from './sseStore';
import { taskStore } from './taskStore';
import { epicStore } from './epicStore';
import { projectStore } from './projectStore';

vi.mock('@/lib/queryClient', () => ({
  queryClient: {
    setQueryData: vi.fn(),
    invalidateQueries: vi.fn(),
  },
}));

vi.mock('@/api/server', () => ({
  fetchProjects: vi.fn().mockResolvedValue([]),
}));

describe('sseEventHandlers', () => {
  beforeEach(() => {
    taskStore.getState().clearTasks();
    epicStore.getState().clearEpics();
    projectStore.setState({ selectedProjectPath: null, projects: [] });
  });

  it('routes task created/updated/deleted to taskStore', () => {
    const cleanup = initSSEEventHandlers();

    sseStore.getState().emit({ type: 'task_created', data: { data: { id: 't1', title: 'A', status: 'open' } }, timestamp: 1 });
    expect(taskStore.getState().getTask('t1')).toBeTruthy();

    sseStore.getState().emit({ type: 'task_updated', data: { data: { id: 't1', title: 'B', status: 'in_progress' } }, timestamp: 2 });
    expect(taskStore.getState().getTask('t1')?.title).toBe('B');

    sseStore.getState().emit({ type: 'task_deleted', data: { data: { id: 't1' } }, timestamp: 3 });
    expect(taskStore.getState().getTask('t1')).toBeUndefined();

    cleanup();
  });

  it('routes epic created/updated/deleted to epicStore', () => {
    const cleanup = initSSEEventHandlers();
    sseStore.getState().emit({ type: 'epic_created', data: { data: { id: 'e1', title: 'E' } }, timestamp: 1 });
    expect(epicStore.getState().getEpic('e1')).toBeTruthy();

    sseStore.getState().emit({ type: 'epic_updated', data: { data: { id: 'e1', title: 'E2' } }, timestamp: 2 });
    expect(epicStore.getState().getEpic('e1')?.title).toBe('E2');

    sseStore.getState().emit({ type: 'epic_deleted', data: { data: { id: 'e1' } }, timestamp: 3 });
    expect(epicStore.getState().getEpic('e1')).toBeUndefined();
    cleanup();
  });
});

