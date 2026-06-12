import { renderHook, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useDispatchPauseHydration } from './useDispatchPauseHydration';
import type { ConnectionStatus } from './useServerHealth';
import { refreshDispatchPauseStatus } from '@/stores/dispatchPauseStore';

vi.mock('@/stores/dispatchPauseStore', () => ({
  refreshDispatchPauseStatus: vi.fn().mockResolvedValue(undefined),
}));

describe('useDispatchPauseHydration', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('hydrates once when server status becomes connected and again after reconnect', async () => {
    const { rerender } = renderHook(({ status }) => useDispatchPauseHydration(status), {
      initialProps: { status: 'loading' as ConnectionStatus },
    });

    expect(refreshDispatchPauseStatus).not.toHaveBeenCalled();

    rerender({ status: 'connected' });
    await waitFor(() => expect(refreshDispatchPauseStatus).toHaveBeenCalledTimes(1));

    rerender({ status: 'connected' });
    expect(refreshDispatchPauseStatus).toHaveBeenCalledTimes(1);

    rerender({ status: 'error' });
    rerender({ status: 'connected' });
    await waitFor(() => expect(refreshDispatchPauseStatus).toHaveBeenCalledTimes(2));
  });
});
