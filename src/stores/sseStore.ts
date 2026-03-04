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
  | `epic_${string}`
  | `project_${string}`;

export interface SSEEvent {
  type: SSEEventType;
  data: unknown;
  timestamp: number;
}

export interface SSEState {
  isConnected: boolean;
  lastError: Error | null;
  reconnectAttempt: number;
  handlers: Map<SSEEventType, Set<(event: SSEEvent) => void>>;
  setConnected: (connected: boolean) => void;
  setError: (error: Error | null) => void;
  incrementReconnectAttempt: () => void;
  resetReconnectAttempt: () => void;
  subscribe: (eventType: SSEEventType, handler: (event: SSEEvent) => void) => () => void;
  emit: (event: SSEEvent) => void;
}

export const sseStore = createStore<SSEState>((set, get) => ({
  isConnected: false,
  lastError: null,
  reconnectAttempt: 0,
  handlers: new Map(),

  setConnected: (connected) => set({ isConnected: connected }),
  
  setError: (error) => set({ lastError: error }),
  
  incrementReconnectAttempt: () => 
    set((state) => ({ reconnectAttempt: state.reconnectAttempt + 1 })),
  
  resetReconnectAttempt: () => set({ reconnectAttempt: 0 }),
  
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
