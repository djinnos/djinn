/**
 * SSE Store - Vanilla createStore for SSE lifecycle (outside React)
 * 
 * Manages EventSource connection state and event handlers.
 * The store is created outside React to prevent unnecessary re-renders.
 */

import { createStore } from "zustand/vanilla";

export type SSEEventType =
  | "task_created"
  | "task_updated"
  | "task_deleted"
  | "epic_created"
  | "epic_updated"
  | "epic_deleted"
  | "project_changed"
  | "session_message"
  | "sync_completed";

export type ConnectionStatus = "connected" | "reconnecting" | "error";

export interface SSEEvent {
  type: SSEEventType;
  data: unknown;
  timestamp: number;
  id?: string;
}

export interface SSEState {
  isConnected: boolean;
  connectionStatus: ConnectionStatus;
  lastError: Error | null;
  reconnectAttempt: number;
  lastEventId: string | null;
  handlers: Map<SSEEventType, Set<(event: SSEEvent) => void>>;
  setConnected: (connected: boolean) => void;
  setConnectionStatus: (status: ConnectionStatus) => void;
  setError: (error: Error | null) => void;
  incrementReconnectAttempt: () => void;
  resetReconnectAttempt: () => void;
  setLastEventId: (id: string | null) => void;
  subscribe: (eventType: SSEEventType, handler: (event: SSEEvent) => void) => () => void;
  emit: (event: SSEEvent) => void;
}

export const sseStore = createStore<SSEState>((set, get) => ({
  isConnected: false,
  connectionStatus: "reconnecting",
  lastError: null,
  reconnectAttempt: 0,
  lastEventId: null,
  handlers: new Map(),

  setConnected: (connected) => set({ isConnected: connected }),
  
  setConnectionStatus: (status) => set({ connectionStatus: status }),
  
  setError: (error) => set({ lastError: error }),
  
  incrementReconnectAttempt: () => 
    set((state) => ({ reconnectAttempt: state.reconnectAttempt + 1 })),
  
  resetReconnectAttempt: () => set({ reconnectAttempt: 0 }),
  
  setLastEventId: (id) => set({ lastEventId: id }),
  
  subscribe: (eventType, handler) => {
    const { handlers } = get();
    if (!handlers.has(eventType)) {
      handlers.set(eventType, new Set());
    }
    const handlerSet = handlers.get(eventType);
    if (handlerSet) {
      handlerSet.add(handler);
    }
    
    return () => {
      handlers.get(eventType)?.delete(handler);
    };
  },
  
  emit: (event) => {
    const { handlers } = get();
    const eventHandlers = handlers.get(event.type);
    if (eventHandlers) {
      eventHandlers.forEach((handler) => handler(event));
    }
  },
}));
