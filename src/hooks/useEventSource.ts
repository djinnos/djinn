/**
 * useEventSource hook - Manages EventSource connection with auto-reconnect
 *
 * Features:
 * - Stores EventSource in useRef to prevent re-renders
 * - Connects to http://127.0.0.1:{port}/events on startup
 * - Exponential backoff on connection errors
 * - Parses SSE event types: task_*, epic_*, project_* and emits to sseStore
 * - Tracks Last-Event-ID for replay on reconnect
 * - Manages connection status: connected | reconnecting | error
 */

import { useEffect, useRef } from "react";
import { sseStore, type SSEEvent, type SSEEventType } from "../stores/sseStore";
import { getServerPort } from "../tauri";
import { initSSEEventHandlers } from "../stores/sseEventHandlers";
import { fetchKanbanSnapshot } from "@/api/server";
import { taskStore } from "@/stores/taskStore";
import { epicStore } from "@/stores/epicStore";

const INITIAL_RECONNECT_DELAY = 1000;
const MAX_RECONNECT_DELAY = 30000;
const RECONNECT_MULTIPLIER = 2;
const MAX_RECONNECT_ATTEMPTS = 10;

export function useEventSource() {
  const eventSourceRef = useRef<EventSource | null>(null);
  const reconnectTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const cleanupHandlersRef = useRef<(() => void) | null>(null);
  const snapshotLoadRef = useRef<Promise<void> | null>(null);

  const hydrateSnapshot = async () => {
    if (snapshotLoadRef.current) {
      return snapshotLoadRef.current;
    }

    snapshotLoadRef.current = (async () => {
      try {
        const snapshot = await fetchKanbanSnapshot();
        taskStore.getState().setTasks(snapshot.tasks);
        epicStore.getState().setEpics(snapshot.epics);
      } catch (error) {
        console.error("Failed to hydrate Kanban snapshot:", error);
      }
    })();

    try {
      await snapshotLoadRef.current;
    } finally {
      snapshotLoadRef.current = null;
    }
  };

  const normalizeEventType = (rawType: string): SSEEventType | null => {
    const normalized = rawType.replace(".", "_");
    if (normalized === "task_created") return "task_created";
    if (normalized === "task_updated") return "task_updated";
    if (normalized === "task_deleted") return "task_deleted";
    if (normalized === "epic_created") return "epic_created";
    if (normalized === "epic_updated") return "epic_updated";
    if (normalized === "epic_deleted") return "epic_deleted";
    if (
      normalized === "project_changed" ||
      normalized === "project_created" ||
      normalized === "project_updated" ||
      normalized === "project_deleted" ||
      normalized === "project_health_ok" ||
      normalized === "project_health_error"
    ) {
      return "project_changed";
    }
    return null;
  };

  useEffect(() => {
    let isActive = true;

    // Initialize SSE event handlers (wire stores to SSE events)
    cleanupHandlersRef.current = initSSEEventHandlers();

    const connect = async () => {
      try {
        await hydrateSnapshot();

        const port = await getServerPort();
        
        // Build URL with Last-Event-ID if available
        let url = `http://127.0.0.1:${port}/events`;
        const lastEventId = sseStore.getState().lastEventId;
        if (lastEventId) {
          url += `?lastEventId=${encodeURIComponent(lastEventId)}`;
        }

        if (!isActive) return;

        const es = new EventSource(url);
        eventSourceRef.current = es;

        es.onopen = () => {
          if (!isActive) return;
          if (sseStore.getState().reconnectAttempt > 0) {
            void hydrateSnapshot();
          }
          sseStore.getState().resetReconnectAttempt();
          sseStore.getState().setConnected(true);
          sseStore.getState().setConnectionStatus("connected");
          sseStore.getState().setError(null);
        };

        const eventTypes = [
          "lagged",
          "task_created",
          "task_updated",
          "task_deleted",
          "epic_created",
          "epic_updated",
          "epic_deleted",
          "project_changed",
          "task.created",
          "task.updated",
          "task.deleted",
          "epic.created",
          "epic.updated",
          "epic.deleted",
          "project.created",
          "project.updated",
          "project.deleted",
          "project.health_ok",
          "project.health_error",
        ] as const;

        eventTypes.forEach((eventType) => {
          es.addEventListener(eventType, (event) => {
            if (!isActive) return;
            try {
              if (eventType === "lagged") {
                void hydrateSnapshot();
                return;
              }

              const data = JSON.parse(event.data);
              
              // Track the event ID from the SSE message if present
              const eventId = (event as MessageEvent).lastEventId || undefined;
              if (eventId) {
                sseStore.getState().setLastEventId(eventId);
              }
              
              const mappedType = normalizeEventType(eventType);
              if (!mappedType) {
                return;
              }

              const sseEvent: SSEEvent = {
                type: mappedType,
                data,
                timestamp: Date.now(),
                id: eventId,
              };
              sseStore.getState().emit(sseEvent);
            } catch (err) {
              console.error(`Failed to parse ${eventType} event:`, err);
            }
          });
        });

        es.onerror = () => {
          if (!isActive) return;
          
          sseStore.getState().setConnected(false);
          
          const { reconnectAttempt } = sseStore.getState();
          
          if (reconnectAttempt >= MAX_RECONNECT_ATTEMPTS) {
            // Max retries reached, set error status
            sseStore.getState().setConnectionStatus("error");
            sseStore.getState().setError(new Error("EventSource max reconnect attempts reached"));
          } else {
            // Still trying to reconnect
            sseStore.getState().setConnectionStatus("reconnecting");
            sseStore.getState().setError(new Error("EventSource connection error"));
          }
          
          es.close();
          eventSourceRef.current = null;

          // Only schedule reconnect if we haven't hit max attempts
          if (reconnectAttempt < MAX_RECONNECT_ATTEMPTS) {
            const delay = Math.min(
              INITIAL_RECONNECT_DELAY * Math.pow(RECONNECT_MULTIPLIER, reconnectAttempt),
              MAX_RECONNECT_DELAY
            );
            sseStore.getState().incrementReconnectAttempt();

            reconnectTimerRef.current = setTimeout(() => {
              if (isActive) {
                connect();
              }
            }, delay);
          }
        };
      } catch (err) {
        if (!isActive) return;
        console.error("Failed to connect to EventSource:", err);
        sseStore.getState().setConnectionStatus("error");
        sseStore.getState().setError(err instanceof Error ? err : new Error(String(err)));
      }
    };

    connect();

    return () => {
      isActive = false;
      if (reconnectTimerRef.current) {
        clearTimeout(reconnectTimerRef.current);
      }
      if (eventSourceRef.current) {
        eventSourceRef.current.close();
        eventSourceRef.current = null;
      }
      sseStore.getState().setConnected(false);
      
      // Cleanup SSE event handlers
      if (cleanupHandlersRef.current) {
        cleanupHandlersRef.current();
        cleanupHandlersRef.current = null;
      }
    };
  }, []);

  return {
    eventSource: eventSourceRef.current,
  };
}
