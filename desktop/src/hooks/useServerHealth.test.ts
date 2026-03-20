import { renderHook, waitFor, act, cleanup } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const tauriCommandMocks = vi.hoisted(() => ({
  getServerStatus: vi.fn(),
  retryServerDiscovery: vi.fn(),
}));

const tauriEventMocks = vi.hoisted(() => ({
  listen: vi.fn(),
}));

vi.mock('@/tauri/commands', () => ({
  getServerStatus: tauriCommandMocks.getServerStatus,
  retryServerDiscovery: tauriCommandMocks.retryServerDiscovery,
}));
vi.mock('@tauri-apps/api/event', () => ({ listen: tauriEventMocks.listen }));

import { useServerHealth } from './useServerHealth';

describe('useServerHealth', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.useRealTimers();
    tauriEventMocks.listen.mockResolvedValue(vi.fn());
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
    vi.useRealTimers();
  });

  it('sets connected state when server is healthy', async () => {
    tauriCommandMocks.getServerStatus.mockResolvedValue({ is_healthy: true, port: 7777, has_error: false });
    const { result, unmount } = renderHook(() => useServerHealth());

    await waitFor(() => expect(result.current.status).toBe('connected'));
    expect(result.current.port).toBe(7777);
    expect(result.current.error).toBeNull();
    unmount();
  });

  it('sets disconnected/error state when server reports error', async () => {
    tauriCommandMocks.getServerStatus.mockResolvedValue({ is_healthy: false, port: null, has_error: true, error_message: 'down' });
    const { result, unmount } = renderHook(() => useServerHealth());

    await waitFor(() => expect(result.current.status).toBe('error'));
    expect(result.current.error).toBe('down');
    unmount();
  });

  it('polls until connected', async () => {
    vi.useFakeTimers();
    tauriCommandMocks.getServerStatus
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
