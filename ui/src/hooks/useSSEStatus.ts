/**
 * useSSEStatus hook - React hook for SSE connection status
 *
 * Returns the current connection status and reconnect attempt count
 * from the sseStore for use in React components.
 */

import { useEffect, useState } from "react";
import { sseStore, type ConnectionStatus } from "../stores/sseStore";

interface SSEStatus {
  status: ConnectionStatus;
  reconnectAttempt: number;
  isConnected: boolean;
}

export function useSSEStatus(): SSEStatus {
  const [status, setStatus] = useState<SSEStatus>(() => {
    const state = sseStore.getState();
    return {
      status: state.connectionStatus,
      reconnectAttempt: state.reconnectAttempt,
      isConnected: state.isConnected,
    };
  });

  useEffect(() => {
    // Subscribe to all state changes
    const unsubscribe = sseStore.subscribe((state) => {
      setStatus({
        status: state.connectionStatus,
        reconnectAttempt: state.reconnectAttempt,
        isConnected: state.isConnected,
      });
    });

    return unsubscribe;
  }, []);

  return status;
}
