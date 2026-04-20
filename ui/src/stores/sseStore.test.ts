import { beforeEach, describe, expect, it, vi } from 'vitest';
import { sseStore, type SSEEvent } from './sseStore';

describe('sseStore', () => {
  beforeEach(() => {
    sseStore.setState({
      isConnected: false,
      connectionStatus: 'reconnecting',
      lastError: null,
      reconnectAttempt: 0,
      lastEventId: null,
      handlers: new Map(),
    });
  });

  it('updates connection lifecycle state', () => {
    const st = sseStore.getState();
    st.setConnected(true);
    st.setConnectionStatus('connected');
    st.setLastEventId('e1');
    st.incrementReconnectAttempt();
    st.incrementReconnectAttempt();
    st.resetReconnectAttempt();
    expect(sseStore.getState().isConnected).toBe(true);
    expect(sseStore.getState().connectionStatus).toBe('connected');
    expect(sseStore.getState().lastEventId).toBe('e1');
    expect(sseStore.getState().reconnectAttempt).toBe(0);
  });

  it('stores and clears errors', () => {
    const err = new Error('boom');
    sseStore.getState().setError(err);
    expect(sseStore.getState().lastError).toBe(err);
    sseStore.getState().setError(null);
    expect(sseStore.getState().lastError).toBeNull();
  });

  it('subscribes, emits, and unsubscribes handlers', () => {
    const handler = vi.fn();
    const unsub = sseStore.getState().subscribe('task_created', handler);
    const event: SSEEvent = { type: 'task_created', data: { id: '1' }, timestamp: Date.now() };
    sseStore.getState().emit(event);
    expect(handler).toHaveBeenCalledTimes(1);
    unsub();
    sseStore.getState().emit(event);
    expect(handler).toHaveBeenCalledTimes(1);
  });
});
