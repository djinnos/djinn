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

  it('routes task created/updated/deleted to taskStore (legacy format)', () => {
    const cleanup = initSSEEventHandlers();

    sseStore.getState().emit({ type: 'task_created', data: { data: { id: 't1', title: 'A', status: 'open' } }, timestamp: 1 });
    expect(taskStore.getState().getTask('t1')).toBeTruthy();

    sseStore.getState().emit({ type: 'task_updated', data: { data: { id: 't1', title: 'B', status: 'in_progress' } }, timestamp: 2 });
    expect(taskStore.getState().getTask('t1')?.title).toBe('B');

    sseStore.getState().emit({ type: 'task_deleted', data: { data: { id: 't1' } }, timestamp: 3 });
    expect(taskStore.getState().getTask('t1')).toBeUndefined();

    cleanup();
  });

  it('routes task events from DjinnEventEnvelope format', () => {
    const cleanup = initSSEEventHandlers();

    // Server sends: {"entity_type":"task","action":"created","payload":{"task":{...},"from_sync":false}}
    sseStore.getState().emit({
      type: 'task_created',
      data: { entity_type: 'task', action: 'created', payload: { task: { id: 't2', title: 'Envelope', status: 'open' }, from_sync: false } },
      timestamp: 1,
    });
    expect(taskStore.getState().getTask('t2')).toBeTruthy();
    expect(taskStore.getState().getTask('t2')?.title).toBe('Envelope');

    sseStore.getState().emit({
      type: 'task_updated',
      data: { entity_type: 'task', action: 'updated', payload: { task: { id: 't2', title: 'Updated', status: 'in_progress' }, from_sync: false } },
      timestamp: 2,
    });
    expect(taskStore.getState().getTask('t2')?.title).toBe('Updated');

    sseStore.getState().emit({
      type: 'task_deleted',
      data: { entity_type: 'task', action: 'deleted', payload: { id: 't2' } },
      timestamp: 3,
    });
    expect(taskStore.getState().getTask('t2')).toBeUndefined();

    cleanup();
  });

  it('routes epic created/updated/deleted to epicStore (legacy format)', () => {
    const cleanup = initSSEEventHandlers();
    sseStore.getState().emit({ type: 'epic_created', data: { data: { id: 'e1', title: 'E' } }, timestamp: 1 });
    expect(epicStore.getState().getEpic('e1')).toBeTruthy();

    sseStore.getState().emit({ type: 'epic_updated', data: { data: { id: 'e1', title: 'E2' } }, timestamp: 2 });
    expect(epicStore.getState().getEpic('e1')?.title).toBe('E2');

    sseStore.getState().emit({ type: 'epic_deleted', data: { data: { id: 'e1' } }, timestamp: 3 });
    expect(epicStore.getState().getEpic('e1')).toBeUndefined();
    cleanup();
  });

  it('routes epic events from DjinnEventEnvelope format', () => {
    const cleanup = initSSEEventHandlers();

    // Epics are serialized flat into payload (no "epic" wrapper key)
    sseStore.getState().emit({
      type: 'epic_created',
      data: { entity_type: 'epic', action: 'created', payload: { id: 'e2', title: 'Epic Env' } },
      timestamp: 1,
    });
    expect(epicStore.getState().getEpic('e2')).toBeTruthy();
    expect(epicStore.getState().getEpic('e2')?.title).toBe('Epic Env');

    sseStore.getState().emit({
      type: 'epic_updated',
      data: { entity_type: 'epic', action: 'updated', payload: { id: 'e2', title: 'Epic Updated' } },
      timestamp: 2,
    });
    expect(epicStore.getState().getEpic('e2')?.title).toBe('Epic Updated');

    sseStore.getState().emit({
      type: 'epic_deleted',
      data: { entity_type: 'epic', action: 'deleted', payload: { id: 'e2' } },
      timestamp: 3,
    });
    expect(epicStore.getState().getEpic('e2')).toBeUndefined();

    cleanup();
  });

  it('routes session_dispatched from envelope to taskStore', () => {
    const cleanup = initSSEEventHandlers();

    // Seed a task first
    sseStore.getState().emit({
      type: 'task_created',
      data: { entity_type: 'task', action: 'created', payload: { task: { id: 't3', title: 'Sess', status: 'open' }, from_sync: false } },
      timestamp: 1,
    });

    sseStore.getState().emit({
      type: 'session_dispatched',
      data: { entity_type: 'session', action: 'dispatched', payload: { task_id: 't3', agent_type: 'worker', model_id: 'openai/gpt-5.3-codex' } },
      timestamp: 2,
    });

    const task = taskStore.getState().getTask('t3');
    expect(task?.active_session).toBeTruthy();
    expect(task?.active_session?.agent_type).toBe('worker');

    cleanup();
  });
});
