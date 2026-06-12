import { beforeEach, describe, expect, it, vi } from 'vitest';
import { dispatchPauseStore, refreshDispatchPauseStatus } from './dispatchPauseStore';
import { fetchDispatchPauseStatus } from '@/api/dispatchPause';

vi.mock('@/api/dispatchPause', () => ({
  fetchDispatchPauseStatus: vi.fn(),
}));

const pause = (reason: string) => ({
  paused_by: 'admin',
  paused_at: '2026-01-01T00:00:00Z',
  reason,
});

describe('dispatchPauseStore', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    dispatchPauseStore.setState({
      global: null,
      projects: {},
      users: {},
      isHydrating: false,
      lastHydratedAt: null,
      lastError: null,
    });
  });

  it('hydrates global, project, and user pause entries from read-only status', async () => {
    vi.mocked(fetchDispatchPauseStatus).mockResolvedValue({
      ok: true,
      state: {
        global: pause('global pause'),
        projects: { 'project-1': pause('project pause') },
        users: { 'user-1': pause('user pause') },
      },
    });

    await refreshDispatchPauseStatus();

    expect(fetchDispatchPauseStatus).toHaveBeenCalledWith();
    expect(dispatchPauseStore.getState().global?.reason).toBe('global pause');
    expect(dispatchPauseStore.getState().projects['project-1']?.target_id).toBe('project-1');
    expect(dispatchPauseStore.getState().users['user-1']?.reason).toBe('user pause');
    expect(dispatchPauseStore.getState().lastError).toBeNull();
  });

  it('upserts pause SSE payloads and clears matching resumed scopes only', () => {
    const store = dispatchPauseStore.getState();
    store.applySsePayload({ scope: 'global', current: pause('global pause') });
    store.applySsePayload({ scope: 'project', target_id: 'project-1', current: pause('project pause') });
    store.applySsePayload({ scope: 'user', target_id: 'user-1', current: pause('user pause') });

    expect(dispatchPauseStore.getState().global?.reason).toBe('global pause');
    expect(dispatchPauseStore.getState().projects['project-1']?.reason).toBe('project pause');
    expect(dispatchPauseStore.getState().users['user-1']?.reason).toBe('user pause');

    dispatchPauseStore.getState().applySsePayload({
      scope: 'project',
      target_id: 'project-1',
      current: null,
      previous: pause('project pause'),
      resumed_by: 'admin-2',
    });

    expect(dispatchPauseStore.getState().global?.reason).toBe('global pause');
    expect(dispatchPauseStore.getState().projects['project-1']).toBeUndefined();
    expect(dispatchPauseStore.getState().users['user-1']?.reason).toBe('user pause');

    dispatchPauseStore.getState().applySsePayload({
      scope: 'global',
      current: null,
      previous: pause('global pause'),
      resumed_by: 'admin-2',
    });
    dispatchPauseStore.getState().applySsePayload({
      scope: 'user',
      target_id: 'user-1',
      current: null,
      previous: pause('user pause'),
      resumed_by: 'admin-2',
    });

    expect(dispatchPauseStore.getState().getAffectedEntries(['project-1'], 'user-1')).toEqual([]);
  });

  it('selects affected global, project, and user entries without conflating scopes', () => {
    const store = dispatchPauseStore.getState();
    store.upsert({ ...pause('global pause'), scope: 'global', target_id: null });
    store.upsert({ ...pause('project pause'), scope: 'project', target_id: 'same-id' });
    store.upsert({ ...pause('user pause'), scope: 'user', target_id: 'same-id' });

    expect(store.getAffectedEntries(['same-id'], 'same-id').map((entry) => entry.scope)).toEqual([
      'global',
      'project',
      'user',
    ]);
    expect(store.getEntry('project', 'same-id')?.reason).toBe('project pause');
    expect(store.getEntry('user', 'same-id')?.reason).toBe('user pause');
  });
});
