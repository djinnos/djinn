import { describe, expect, it, vi, beforeEach } from 'vitest';
import {
  fetchDispatchPauseStatus,
  fetchGlobalDispatchPauseStatus,
  fetchProjectDispatchPauseStatus,
  fetchUserDispatchPauseStatus,
} from './dispatchPause';
import { callMcpTool } from '@/api/mcpClient';

vi.mock('@/api/mcpClient', () => ({
  callMcpTool: vi.fn().mockResolvedValue({ ok: true }),
}));

describe('dispatchPause API wrapper', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('calls only the read-only dispatch_pause_status tool with exact empty args', async () => {
    await fetchDispatchPauseStatus();

    expect(callMcpTool).toHaveBeenCalledTimes(1);
    expect(callMcpTool).toHaveBeenCalledWith('dispatch_pause_status', {});
    expect(vi.mocked(callMcpTool).mock.calls.map(([name]) => name)).not.toContain('dispatch_pause');
    expect(vi.mocked(callMcpTool).mock.calls.map(([name]) => name)).not.toContain('dispatch_resume');
  });

  it('passes only status scope and target_id fields for scoped reads', async () => {
    await fetchProjectDispatchPauseStatus('project-1');
    await fetchUserDispatchPauseStatus('user-1');
    await fetchGlobalDispatchPauseStatus();
    await fetchDispatchPauseStatus({
      scope: 'project',
      target_id: 'project-2',
      reason: 'must not forward',
      paused_by: 'must not forward',
      resume: true,
    } as never);

    expect(callMcpTool).toHaveBeenNthCalledWith(1, 'dispatch_pause_status', {
      scope: 'project',
      target_id: 'project-1',
    });
    expect(callMcpTool).toHaveBeenNthCalledWith(2, 'dispatch_pause_status', {
      scope: 'user',
      target_id: 'user-1',
    });
    expect(callMcpTool).toHaveBeenNthCalledWith(3, 'dispatch_pause_status', { scope: 'global' });
    expect(callMcpTool).toHaveBeenNthCalledWith(4, 'dispatch_pause_status', {
      scope: 'project',
      target_id: 'project-2',
    });
  });
});
