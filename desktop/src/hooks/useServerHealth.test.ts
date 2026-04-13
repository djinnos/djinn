import { renderHook, waitFor, act, cleanup } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const commandMocks = vi.hoisted(() => ({
  getServerStatus: vi.fn(),
  retryServerConnection: vi.fn(),
}));

const eventMocks = vi.hoisted(() => ({
  listen: vi.fn(),
}));

vi.mock('@/electron/commands', () => ({
  getServerStatus: commandMocks.getServerStatus,
  retryServerConnection: commandMocks.retryServerConnection,
}));
vi.mock('@/electron/shims/event', () => ({ listen: eventMocks.listen }));

import { useServerHealth } from './useServerHealth';

describe('useServerHealth', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.useRealTimers();
    eventMocks.listen.mockResolvedValue(vi.fn());
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
    vi.useRealTimers();
  });

  it('sets connected state when server is healthy', async () => {
    commandMocks.getServerStatus.mockResolvedValue({ is_healthy: true, port: 7777, has_error: false });
    const { result, unmount } = renderHook(() => useServerHealth());

    await waitFor(() => expect(result.current.status).toBe('connected'));
    expect(result.current.port).toBe(7777);
    expect(result.current.error).toBeNull();
    unmount();
  });

  it('sets disconnected/error state when server reports error', async () => {
    commandMocks.getServerStatus.mockResolvedValue({ is_healthy: false, port: null, has_error: true, error_message: 'down' });
    const { result, unmount } = renderHook(() => useServerHealth());

    await waitFor(() => expect(result.current.status).toBe('error'));
    expect(result.current.error).toBe('down');
    unmount();
  });

  it('polls until connected', async () => {
    vi.useFakeTimers();
    commandMocks.getServerStatus
      .mockResolvedValueOnce({ is_healthy: false, port: null, has_error: false })
      .mockResolvedValue({ is_healthy: true, port: 9999, has_error: false });

    const { result, unmount } = renderHook(() => useServerHealth());

    await act(async () => {
      await vi.runOnlyPendingTimersAsync();
    });

    await act(async () => {
      vi.advanceTimersByTime(2000);
      await Promise.resolve();
    });

    for (let i = 0; i < 5 && result.current.status !== 'connected'; i += 1) {
      await act(async () => {
        vi.advanceTimersByTime(2000);
        await Promise.resolve();
      });
    }

    expect(result.current.status).toBe('connected');
    expect(result.current.port).toBe(9999);

    unmount();
  });
});
