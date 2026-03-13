import { beforeEach, describe, expect, it, vi } from 'vitest';
import { initSSEEventHandlers } from './sseEventHandlers';
import { sseStore } from './sseStore';
import { taskStore } from './taskStore';
import { epicStore } from './epicStore';
import { projectStore } from './projectStore';
import { projectSessionStore } from './projectSessionStore';

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
    projectSessionStore.getState().clearProjectSessions();
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



it('tracks project-scoped session lifecycle when task_id is null', () => {
  const cleanup = initSSEEventHandlers();

  sseStore.getState().emit({
    type: 'session_dispatched',
    data: { data: { task_id: null, project_id: 'p1', agent_type: 'groomer', model_id: 'm1' } },
    timestamp: 1,
  });

  const dispatched = projectSessionStore.getState().getActiveProjectSession('p1');
  expect(dispatched).toBeTruthy();
  expect(dispatched?.project_id).toBe('p1');
  expect(dispatched?.session_id).toBeUndefined();
  expect(dispatched?.agent_type).toBe('groomer');
  expect(dispatched?.model_id).toBe('m1');
  expect(dispatched?.status).toBe('dispatched');

  sseStore.getState().emit({
    type: 'session_started',
    data: { data: { task_id: null, project_id: 'p1', id: 's1', started_at: '2026-01-01T00:00:00.000Z', status: 'started' } },
    timestamp: 2,
  });

  const started = projectSessionStore.getState().getActiveProjectSession('p1');
  expect(started?.session_id).toBe('s1');
  expect(started?.started_at).toBe('2026-01-01T00:00:00.000Z');
  expect(started?.status).toBe('started');

  sseStore.getState().emit({
    type: 'session_ended',
    data: { data: { task_id: null, project_id: 'p1' } },
    timestamp: 3,
  });

  expect(projectSessionStore.getState().getActiveProjectSession('p1')).toBeUndefined();
  cleanup();
});
