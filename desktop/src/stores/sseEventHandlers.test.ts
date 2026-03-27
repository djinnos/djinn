import { beforeEach, afterEach, describe, expect, it, vi } from 'vitest';
import { initSSEEventHandlers } from './sseEventHandlers';
import { sseStore } from './sseStore';
import { taskStore } from './taskStore';
import { epicStore } from './epicStore';
import { projectStore } from './projectStore';
import { verificationStore } from './verificationStore';
import { fetchProjects } from '@/api/server';
import {
  flushDebouncedInvalidations,
  queryClient,
  SSE_QUERY_DEBOUNCE_MS,
} from '@/lib/queryClient';

vi.mock('@/lib/queryClient', () => {
  type PendingInvalidation = {
    filters: { queryKey?: unknown[] };
    timer: ReturnType<typeof setTimeout>;
  };

  const invalidateQueries = vi.fn();
  const pendingInvalidations = new Map<string, PendingInvalidation>();

  const runInvalidation = (filters: { queryKey?: unknown[] }) => {
    invalidateQueries(filters);
  };

  return {
    SSE_QUERY_DEBOUNCE_MS: 150,
    queryClient: {
      setQueryData: vi.fn(),
      invalidateQueries,
    },
    debounceInvalidateQueries: vi.fn((filters: { queryKey?: unknown[] }) => {
      if (!filters.queryKey) {
        runInvalidation(filters);
        return;
      }

      const key = JSON.stringify(filters.queryKey);
      const existing = pendingInvalidations.get(key);
      if (existing) {
        clearTimeout(existing.timer);
      }

      const timer = setTimeout(() => {
        const pending = pendingInvalidations.get(key);
        pendingInvalidations.delete(key);
        runInvalidation(pending?.filters ?? filters);
      }, 150);

      pendingInvalidations.set(key, { filters, timer });
    }),
    flushDebouncedInvalidations: vi.fn(() => {
      const pending = Array.from(pendingInvalidations.values());
      pendingInvalidations.clear();

      for (const entry of pending) {
        clearTimeout(entry.timer);
        runInvalidation(entry.filters);
      }
    }),
  };
});

vi.mock('@/api/server', () => ({
  fetchProjects: vi.fn().mockResolvedValue([]),
}));

describe('sseEventHandlers', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.clearAllMocks();
    taskStore.getState().clearTasks();
    epicStore.getState().clearEpics();
    verificationStore.setState({ runs: new Map(), lifecycle: new Map() });
    projectStore.setState({ selectedProjectId: null, projects: [] });
  });

  afterEach(() => {
    vi.clearAllTimers();
    vi.useRealTimers();
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

  it('debounces sync-triggered query invalidations across bursts', () => {
    const cleanup = initSSEEventHandlers();

    sseStore.getState().emit({
      type: 'sync_completed',
      data: { entity_type: 'sync', action: 'completed', payload: { direction: 'import', count: 2 } },
      timestamp: 1,
    });
    sseStore.getState().emit({
      type: 'sync_completed',
      data: { entity_type: 'sync', action: 'completed', payload: { direction: 'import', count: 4 } },
      timestamp: 2,
    });

    expect(queryClient.invalidateQueries).not.toHaveBeenCalled();

    vi.advanceTimersByTime(SSE_QUERY_DEBOUNCE_MS - 1);
    expect(queryClient.invalidateQueries).not.toHaveBeenCalled();

    vi.advanceTimersByTime(1);
    expect(queryClient.invalidateQueries).toHaveBeenCalledTimes(2);
    expect(queryClient.invalidateQueries).toHaveBeenNthCalledWith(1, { queryKey: ['tasks'] });
    expect(queryClient.invalidateQueries).toHaveBeenNthCalledWith(2, { queryKey: ['epics'] });

    cleanup();
  });

  it('coalesces project refresh invalidations while still updating project store immediately', async () => {
    const cleanup = initSSEEventHandlers();
    const projects = [{ id: 'p1', name: 'Proj', path: '/tmp/proj' }];
    vi.mocked(fetchProjects).mockResolvedValue(projects as never);

    sseStore.getState().emit({
      type: 'project_changed',
      data: { entity_type: 'project', action: 'updated', payload: { id: 'p1' } },
      timestamp: 1,
    });
    sseStore.getState().emit({
      type: 'project_changed',
      data: { entity_type: 'project', action: 'updated', payload: { id: 'p1' } },
      timestamp: 2,
    });

    expect(fetchProjects).toHaveBeenCalledTimes(2);
    await Promise.resolve();
    await Promise.resolve();
    expect(projectStore.getState().projects).toEqual(projects);
    expect(queryClient.invalidateQueries).not.toHaveBeenCalled();

    vi.advanceTimersByTime(SSE_QUERY_DEBOUNCE_MS);
    expect(queryClient.invalidateQueries).toHaveBeenCalledTimes(2);
    expect(queryClient.invalidateQueries).toHaveBeenNthCalledWith(1, { queryKey: ['providers'] });
    expect(queryClient.invalidateQueries).toHaveBeenNthCalledWith(2, { queryKey: ['settings'] });

    cleanup();
  });

  it('flushes pending debounced invalidations during cleanup', () => {
    const cleanup = initSSEEventHandlers();

    sseStore.getState().emit({
      type: 'sync_completed',
      data: { entity_type: 'sync', action: 'completed', payload: { direction: 'import', count: 1 } },
      timestamp: 1,
    });

    expect(queryClient.invalidateQueries).not.toHaveBeenCalled();

    cleanup();

    expect(queryClient.invalidateQueries).toHaveBeenCalledTimes(2);
    expect(queryClient.invalidateQueries).toHaveBeenNthCalledWith(1, { queryKey: ['tasks'] });
    expect(queryClient.invalidateQueries).toHaveBeenNthCalledWith(2, { queryKey: ['epics'] });
  });

  it('preserves live session visibility while task updates arrive in bursts', () => {
    const cleanup = initSSEEventHandlers();

    sseStore.getState().emit({
      type: 'task_created',
      data: {
        entity_type: 'task',
        action: 'created',
        payload: { task: { id: 't4', title: 'Burst task', status: 'open', project_id: 'p1' }, from_sync: false },
      },
      timestamp: 1,
    });

    sseStore.getState().emit({
      type: 'session_started',
      data: {
        entity_type: 'session',
        action: 'started',
        payload: { id: 's1', task_id: 't4', agent_type: 'worker', model_id: 'gpt', started_at: '2024-01-01T00:00:00Z', status: 'running' },
      },
      timestamp: 2,
    });

    sseStore.getState().emit({
      type: 'task_updated',
      data: {
        entity_type: 'task',
        action: 'updated',
        payload: { task: { id: 't4', title: 'Burst task 1', status: 'in_progress', project_id: 'p1' }, from_sync: true },
      },
      timestamp: 3,
    });
    sseStore.getState().emit({
      type: 'task_updated',
      data: {
        entity_type: 'task',
        action: 'updated',
        payload: { task: { id: 't4', title: 'Burst task 2', status: 'in_progress', project_id: 'p1' }, from_sync: true },
      },
      timestamp: 4,
    });

    const task = taskStore.getState().getTask('t4');
    expect(task?.title).toBe('Burst task 2');
    expect(task?.active_session?.session_id).toBe('s1');
    expect(queryClient.setQueryData).toHaveBeenCalled();

    cleanup();
  });
});
