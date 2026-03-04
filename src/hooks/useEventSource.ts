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

const INITIAL_RECONNECT_DELAY = 1000;
const MAX_RECONNECT_DELAY = 30000;
const RECONNECT_MULTIPLIER = 2;
const MAX_RECONNECT_ATTEMPTS = 10;

export function useEventSource() {
  const eventSourceRef = useRef<EventSource | null>(null);
  const reconnectTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    let isActive = true;

    const connect = async () => {
      try {
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
          sseStore.getState().resetReconnectAttempt();
          sseStore.getState().setConnected(true);
          sseStore.getState().setConnectionStatus("connected");
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
              
              // Track the event ID from the SSE message if present
              const eventId = (event as MessageEvent).lastEventId || undefined;
              if (eventId) {
                sseStore.getState().setLastEventId(eventId);
              }
              
              const sseEvent: SSEEvent = {
                type: eventType,
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
    };
  }, []);

  return {
    eventSource: eventSourceRef.current,
  };
}
