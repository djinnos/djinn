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
import { useSelectedProject } from "@/stores/useProjectStore";
import { projectStore, ALL_PROJECTS } from "@/stores/projectStore";
import { taskStore } from "@/stores/taskStore";
import { epicStore } from "@/stores/epicStore";
import { resetMcpClient } from "@/api/mcpClient";
import { listen } from "@tauri-apps/api/event";

const INITIAL_RECONNECT_DELAY = 1000;
const MAX_RECONNECT_DELAY = 30000;
const RECONNECT_MULTIPLIER = 2;
const MAX_RECONNECT_ATTEMPTS = 10;

export function useEventSource(projectId?: string | null) {
  const selectedProject = useSelectedProject();
  const isAll = projectId === ALL_PROJECTS;
  const selectedProjectPath = isAll ? null : (selectedProject?.path ?? null);
  const eventSourceRef = useRef<EventSource | null>(null);
  const reconnectTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const cleanupHandlersRef = useRef<(() => void) | null>(null);
  const normalizeEventType = (rawType: string): SSEEventType | null => {
    const normalized = rawType.replace(".", "_");
    if (normalized === "task_created") return "task_created";
    if (normalized === "task_updated") return "task_updated";
    if (normalized === "task_deleted") return "task_deleted";
    if (normalized === "epic_created") return "epic_created";
    if (normalized === "epic_updated") return "epic_updated";
    if (normalized === "epic_deleted") return "epic_deleted";
    if (normalized === "session_message") return "session_message";
    if (normalized === "session_dispatched") return "session_dispatched";
    if (normalized === "session_started") return "session_started";
    if (
      normalized === "session_completed" ||
      normalized === "session_interrupted" ||
      normalized === "session_failed" ||
      normalized === "session_updated"
    ) {
      return "session_ended";
    }
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
    if (normalized === "sync_completed") return "sync_completed";
    if (normalized === "verification_step") return "verification_step";
    if (normalized === "lifecycle_step") return "lifecycle_step";
    return null;
  };

  useEffect(() => {
    let isActive = true;

    // Clear stores immediately so stale data from previous project isn't shown
    taskStore.getState().clearTasks();
    epicStore.getState().clearEpics();

    const hydrateSnapshot = async (projectPath: string | null) => {
      try {
        const allPaths = isAll
          ? projectStore.getState().projects.map((p) => p.path).filter(Boolean) as string[]
          : undefined;
        const snapshot = await fetchKanbanSnapshot(projectPath, allPaths);
        if (!isActive) return;
        taskStore.getState().setTasks(snapshot.tasks);
        epicStore.getState().setEpics(snapshot.epics);
      } catch (error) {
        console.error("Failed to hydrate Kanban snapshot:", error);
      }
    };

    // Initialize SSE event handlers (wire stores to SSE events)
    cleanupHandlersRef.current = initSSEEventHandlers();

    // When the project path isn't available yet (projects still loading),
    // subscribe directly to the project store so we hydrate as soon as
    // the path resolves — without relying on a React re-render to re-run
    // this effect.
    let unsubProjectPath: (() => void) | undefined;
    if (!selectedProjectPath && projectId) {
      unsubProjectPath = projectStore.subscribe((state) => {
        if (!isActive) return;
        if (isAll) {
          // ALL_PROJECTS mode: wait for projects to load, then hydrate all
          const paths = state.projects.map((p) => p.path).filter(Boolean) as string[];
          if (paths.length > 0) {
            unsubProjectPath?.();
            unsubProjectPath = undefined;
            void hydrateSnapshot(null);
          }
        } else {
          const project = state.getSelectedProject();
          if (project?.path) {
            unsubProjectPath?.();
            unsubProjectPath = undefined;
            void hydrateSnapshot(project.path);
          }
        }
      });
    }

    const connect = async () => {
      try {
        await hydrateSnapshot(selectedProjectPath);

        const port = await getServerPort();
        
        // Build URL with Last-Event-ID if available
        let url = `http://127.0.0.1:${port}/events`;
        if (projectId && !isAll) {
          url += `?project_id=${encodeURIComponent(projectId)}`;
        }
        const lastEventId = sseStore.getState().lastEventId;
        if (lastEventId) {
          url += (url.includes("?") ? "&" : "?") + `lastEventId=${encodeURIComponent(lastEventId)}`;
        }

        if (!isActive) return;

        const es = new EventSource(url);
        eventSourceRef.current = es;

        es.onopen = () => {
          if (!isActive) return;
          if (sseStore.getState().reconnectAttempt > 0) {
            const currentPath = projectStore.getState().getSelectedProject()?.path ?? selectedProjectPath;
            void hydrateSnapshot(currentPath);
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
          "session.message",
          "session.dispatched",
          "session.started",
          "session.completed",
          "session.interrupted",
          "session.failed",
          "session.updated",
          "sync.completed",
          "verification.step",
          "verification_step",
          "lifecycle.step",
          "lifecycle_step",
        ] as const;

        eventTypes.forEach((eventType) => {
          es.addEventListener(eventType, (event) => {
            if (!isActive) return;
            try {
              if (eventType === "lagged") {
                const currentPath = projectStore.getState().getSelectedProject()?.path ?? selectedProjectPath;
                void hydrateSnapshot(currentPath);
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

    // Listen for server reconnection (e.g. after server restart with new port).
    // Reset MCP client cache and force SSE to reconnect to the new port.
    const unlistenReconnected = listen<number>("server:reconnected", async () => {
      if (!isActive) return;

      // Reset MCP client so it picks up the new port
      await resetMcpClient();

      // Close existing SSE and reconnect
      if (reconnectTimerRef.current) {
        clearTimeout(reconnectTimerRef.current);
        reconnectTimerRef.current = null;
      }
      if (eventSourceRef.current) {
        eventSourceRef.current.close();
        eventSourceRef.current = null;
      }

      // Reset reconnect attempt counter so we get a fresh start
      sseStore.getState().resetReconnectAttempt();
      sseStore.getState().setConnectionStatus("reconnecting");

      connect();
    });

    return () => {
      isActive = false;
      unsubProjectPath?.();
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

      unlistenReconnected.then((fn) => fn());
    };
  }, [projectId, selectedProjectPath]);

  return {
    eventSource: eventSourceRef.current,
  };
}
