/**
 * useEventSource hook - Manages EventSource connection with auto-reconnect
 *
 * Features:
 * - Stores EventSource in useRef to prevent re-renders
 * - Connects to http://127.0.0.1:{port}/events on startup
 * - Exponential backoff on connection errors
 * - Parses SSE event types: task_*, epic_*, project_* and emits to sseStore
 */

import { useEffect, useRef } from "react";
import { sseStore, type SSEEvent, type SSEEventType } from "../stores/sseStore";
import { getServerPort } from "../tauri";

const INITIAL_RECONNECT_DELAY = 1000;
const MAX_RECONNECT_DELAY = 30000;
const RECONNECT_MULTIPLIER = 2;

export function useEventSource() {
  const eventSourceRef = useRef<EventSource | null>(null);
  const reconnectTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    let isActive = true;

    const connect = async () => {
      try {
        const port = await getServerPort();
        const url = `http://127.0.0.1:${port}/events`;

        if (!isActive) return;

        const es = new EventSource(url);
        eventSourceRef.current = es;

        es.onopen = () => {
          if (!isActive) return;
          sseStore.getState().resetReconnectAttempt();
          sseStore.getState().setConnected(true);
          sseStore.getState().setError(null);
        };

        // Handle specific event types
        const eventTypes: SSEEventType[] = [
          "task_created",
          "task_updated",
          "task_deleted",
          "epic_created",
          "epic_updated",
          "project_changed",
        ];

        eventTypes.forEach((eventType) => {
          es.addEventListener(eventType, (event) => {
            if (!isActive) return;
            try {
              const data = JSON.parse(event.data);
              const sseEvent: SSEEvent = {
                type: eventType,
                data,
                timestamp: Date.now(),
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
          sseStore.getState().setError(new Error("EventSource connection error"));
          
          es.close();
          eventSourceRef.current = null;

          const { reconnectAttempt } = sseStore.getState();
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
        };
      } catch (err) {
        if (!isActive) return;
        console.error("Failed to connect to EventSource:", err);
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
    };
  }, []);

  return {
    eventSource: eventSourceRef.current,
  };
}
