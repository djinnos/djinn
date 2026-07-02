import { beforeEach, afterEach, describe, expect, it, vi } from 'vitest';
import { initSSEEventHandlers } from './sseEventHandlers';
import { sseStore } from './sseStore';
import { taskStore } from './taskStore';
import { epicStore } from './epicStore';
import { proposalStore } from './proposalStore';
import { projectStore } from './projectStore';
import { dispatchPauseStore } from './dispatchPauseStore';
import { fetchProjects } from '@/api/server';
import {
  flushDebouncedInvalidations,
  queryClient,
  SSE_QUERY_DEBOUNCE_MS,
} from '@/lib/queryClient';
import {
  SERVER_SSE_EVENT_NAMES,
  resolveServerSSEEventName,
} from './sseEventContract';

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
    proposalStore.getState().clearProposals();
    dispatchPauseStore.getState().clearAll();
    projectStore.setState({ selectedProjectId: null, projects: [] });
  });

  afterEach(() => {
    flushDebouncedInvalidations();
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
    const createdMergeCommitSha = 'abc123def4567890abc123def4567890abc123de';
    const updatedMergeCommitSha = 'def456abc1237890def456abc1237890def456ab';

    sseStore.getState().emit({
      type: 'task_created',
      data: {
        entity_type: 'task',
        action: 'created',
        payload: {
          task: {
            id: 't2',
            title: 'Envelope',
            status: 'open',
            merge_commit_sha: createdMergeCommitSha,
          },
          from_sync: false,
        },
      },
      timestamp: 1,
    });
    expect(taskStore.getState().getTask('t2')).toBeTruthy();
    expect(taskStore.getState().getTask('t2')?.title).toBe('Envelope');
    expect(taskStore.getState().getTask('t2')?.merge_commit_sha).toBe(createdMergeCommitSha);

    sseStore.getState().emit({
      type: 'task_updated',
      data: {
        entity_type: 'task',
        action: 'updated',
        payload: {
          task: {
            id: 't2',
            title: 'Updated',
            status: 'in_progress',
            merge_commit_sha: updatedMergeCommitSha,
          },
          from_sync: false,
        },
      },
      timestamp: 2,
    });
    expect(taskStore.getState().getTask('t2')?.title).toBe('Updated');
    expect(taskStore.getState().getTask('t2')?.merge_commit_sha).toBe(updatedMergeCommitSha);

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

  it('adr-045 collapses bursty sync-triggered query invalidations into one refetch per key', () => {
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

  it('adr-045 coalesces project refresh invalidations while still updating project store immediately', async () => {
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

  it('adr-045 keeps store state converged across bursty task updates while preserving live session visibility', () => {
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

  it('adr-045 converges bursty epic updates without redundant invalidations', () => {
    const cleanup = initSSEEventHandlers();

    sseStore.getState().emit({
      type: 'epic_created',
      data: { entity_type: 'epic', action: 'created', payload: { id: 'e-burst', title: 'Epic 0', project_id: 'p1' } },
      timestamp: 1,
    });

    sseStore.getState().emit({
      type: 'sync_completed',
      data: { entity_type: 'sync', action: 'completed', payload: { direction: 'import', count: 3 } },
      timestamp: 2,
    });
    sseStore.getState().emit({
      type: 'epic_updated',
      data: { entity_type: 'epic', action: 'updated', payload: { id: 'e-burst', title: 'Epic 1', project_id: 'p1' } },
      timestamp: 3,
    });
    sseStore.getState().emit({
      type: 'sync_completed',
      data: { entity_type: 'sync', action: 'completed', payload: { direction: 'import', count: 1 } },
      timestamp: 4,
    });
    sseStore.getState().emit({
      type: 'epic_updated',
      data: { entity_type: 'epic', action: 'updated', payload: { id: 'e-burst', title: 'Epic 2', project_id: 'p1' } },
      timestamp: 5,
    });

    expect(epicStore.getState().getEpic('e-burst')?.title).toBe('Epic 2');
    expect(queryClient.invalidateQueries).not.toHaveBeenCalled();

    vi.advanceTimersByTime(SSE_QUERY_DEBOUNCE_MS);
    expect(queryClient.invalidateQueries).toHaveBeenCalledTimes(2);
    expect(queryClient.invalidateQueries).toHaveBeenNthCalledWith(1, { queryKey: ['tasks'] });
    expect(queryClient.invalidateQueries).toHaveBeenNthCalledWith(2, { queryKey: ['epics'] });

    cleanup();
  });

  it('adr-045 collapses session and project burst traffic into one debounced invalidation per query key', async () => {
    const cleanup = initSSEEventHandlers();
    const projects = [{ id: 'p2', name: 'Proj 2', path: '/tmp/proj-2' }];
    vi.mocked(fetchProjects).mockResolvedValue(projects as never);

    sseStore.getState().emit({
      type: 'task_created',
      data: {
        entity_type: 'task',
        action: 'created',
        payload: { task: { id: 'session-task', title: 'Session Task', status: 'open', project_id: 'p2' }, from_sync: false },
      },
      timestamp: 1,
    });

    sseStore.getState().emit({
      type: 'session_started',
      data: {
        entity_type: 'session',
        action: 'started',
        payload: { id: 's-1', task_id: 'session-task', agent_type: 'worker', model_id: 'gpt', started_at: '2024-01-01T00:00:00Z', status: 'running' },
      },
      timestamp: 2,
    });
    sseStore.getState().emit({
      type: 'project_changed',
      data: { entity_type: 'project', action: 'updated', payload: { id: 'p2' } },
      timestamp: 3,
    });
    sseStore.getState().emit({
      type: 'session_ended',
      data: {
        entity_type: 'session',
        action: 'ended',
        payload: { task_id: 'session-task' },
      },
      timestamp: 4,
    });
    sseStore.getState().emit({
      type: 'project_changed',
      data: { entity_type: 'project', action: 'updated', payload: { id: 'p2' } },
      timestamp: 5,
    });

    await Promise.resolve();
    await Promise.resolve();

    const task = taskStore.getState().getTask('session-task');
    expect(task?.active_session).toBeUndefined();
    expect(task?.session_count).toBe(1);
    expect(projectStore.getState().projects).toEqual(projects);
    expect(queryClient.invalidateQueries).not.toHaveBeenCalled();

    vi.advanceTimersByTime(SSE_QUERY_DEBOUNCE_MS);
    expect(queryClient.invalidateQueries).toHaveBeenCalledTimes(2);
    expect(queryClient.invalidateQueries).toHaveBeenNthCalledWith(1, { queryKey: ['providers'] });
    expect(queryClient.invalidateQueries).toHaveBeenNthCalledWith(2, { queryKey: ['settings'] });

    cleanup();
  });

  it('routes dispatch_pause.changed pause and resume envelopes to dispatchPauseStore', () => {
    const cleanup = initSSEEventHandlers();

    sseStore.getState().emit({
      type: 'dispatch_pause_changed',
      data: {
        entity_type: 'dispatch_pause',
        action: 'changed',
        payload: {
          scope: 'project',
          target_id: 'project-1',
          current: {
            paused_by: 'admin-1',
            paused_at: '2026-01-01T00:00:00Z',
            reason: 'maintenance',
          },
          previous: null,
        },
      },
      timestamp: 6,
    });

    expect(dispatchPauseStore.getState().projects['project-1']?.reason).toBe('maintenance');

    sseStore.getState().emit({
      type: 'dispatch_pause_changed',
      data: {
        entity_type: 'dispatch_pause',
        action: 'changed',
        payload: {
          scope: 'project',
          target_id: 'project-1',
          current: null,
          previous: {
            paused_by: 'admin-1',
            paused_at: '2026-01-01T00:00:00Z',
            reason: 'maintenance',
          },
          resumed_by: 'admin-2',
        },
      },
      timestamp: 7,
    });

    expect(dispatchPauseStore.getState().projects['project-1']).toBeUndefined();

    cleanup();
  });

  it('routes proposal.updated envelope to proposalStore and invalidates ["proposals"] (covering detail queries)', () => {
    const cleanup = initSSEEventHandlers();

    // First, seed the store with an existing proposal (simulates prior list fetch)
    proposalStore.getState().addProposal({
      id: 'prop-1',
      title: 'Original Title',
      status: 'draft',
      acceptance_criteria: '[]',
    } as never);

    // Emit a raw DjinnEventEnvelope for proposal.updated
    sseStore.getState().emit({
      type: 'proposal_updated',
      data: {
        entity_type: 'proposal',
        action: 'updated',
        payload: {
          proposal: {
            id: 'prop-1',
            title: 'Updated Title',
            status: 'approved',
            acceptance_criteria: '["Must pass CI"]',
          },
        },
      },
      timestamp: 10,
    });

    // Store is updated immediately (synchronous path)
    const updated = proposalStore.getState().getProposal('prop-1');
    expect(updated).toBeTruthy();
    expect(updated?.title).toBe('Updated Title');
    expect(updated?.status).toBe('approved');

    // The debounced ["proposals"] invalidation hasn't fired yet
    expect(queryClient.invalidateQueries).not.toHaveBeenCalled();

    // After debounce window, ["proposals"] is invalidated once — this covers
    // both ["proposals", "list", ...] and ["proposals", "detail", id] queries,
    // so an open detail page refetches without manual refresh.
    vi.advanceTimersByTime(SSE_QUERY_DEBOUNCE_MS);
    expect(queryClient.invalidateQueries).toHaveBeenCalledTimes(1);
    expect(queryClient.invalidateQueries).toHaveBeenCalledWith({ queryKey: ['proposals'] });

    cleanup();
  });

  it('routes proposal.created and proposal.deleted via the same ["proposals"] invalidation path', () => {
    const cleanup = initSSEEventHandlers();

    // proposal.created
    sseStore.getState().emit({
      type: 'proposal_created',
      data: {
        entity_type: 'proposal',
        action: 'created',
        payload: {
          proposal: {
            id: 'prop-new',
            title: 'New Proposal',
            status: 'draft',
            acceptance_criteria: '[]',
          },
        },
      },
      timestamp: 1,
    });
    expect(proposalStore.getState().getProposal('prop-new')).toBeTruthy();

    // proposal.deleted
    sseStore.getState().emit({
      type: 'proposal_deleted',
      data: {
        entity_type: 'proposal',
        action: 'deleted',
        payload: { id: 'prop-new' },
      },
      timestamp: 2,
    });
    expect(proposalStore.getState().getProposal('prop-new')).toBeUndefined();

    // Both events debounced under the same ["proposals"] key → one invalidation
    vi.advanceTimersByTime(SSE_QUERY_DEBOUNCE_MS);
    expect(queryClient.invalidateQueries).toHaveBeenCalledTimes(1);
    expect(queryClient.invalidateQueries).toHaveBeenCalledWith({ queryKey: ['proposals'] });

    cleanup();
  });

  it('proposal_feedback.created invalidates ["proposals"] to refresh open detail views', () => {
    const cleanup = initSSEEventHandlers();

    sseStore.getState().emit({
      type: 'proposal_feedback_created',
      data: {
        entity_type: 'proposal_feedback',
        action: 'created',
        payload: { proposal_id: 'prop-1', body: 'Looks good', author: 'reviewer-1' },
      },
      timestamp: 1,
    });

    vi.advanceTimersByTime(SSE_QUERY_DEBOUNCE_MS);
    expect(queryClient.invalidateQueries).toHaveBeenCalledTimes(1);
    expect(queryClient.invalidateQueries).toHaveBeenCalledWith({ queryKey: ['proposals'] });

    cleanup();
  });

  it('contract resolves proposal.updated as dispatch → proposal_updated and guards debate-trail events are absent', () => {
    // Verify the SSE contract maps proposal.updated correctly
    expect(resolveServerSSEEventName('proposal.updated')).toEqual({
      kind: 'dispatch',
      eventType: 'proposal_updated',
    });

    // Also verify the full proposal family
    expect(resolveServerSSEEventName('proposal.created')).toEqual({
      kind: 'dispatch',
      eventType: 'proposal_created',
    });
    expect(resolveServerSSEEventName('proposal.deleted')).toEqual({
      kind: 'dispatch',
      eventType: 'proposal_deleted',
    });
    expect(resolveServerSSEEventName('proposal_feedback.created')).toEqual({
      kind: 'dispatch',
      eventType: 'proposal_feedback_created',
    });

    // Guard: debate-trail SSE events must NOT be in the contract
    const eventNames = SERVER_SSE_EVENT_NAMES as readonly string[];
    expect(eventNames).not.toContain('proposal_debate_trail.created');
    expect(eventNames).not.toContain('proposal_debate_trail.updated');
  });
});
