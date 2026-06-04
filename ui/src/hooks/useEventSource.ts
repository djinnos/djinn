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
 *
 * Task/epic data is hydrated for ALL projects unconditionally. Per-page
 * filtering (Kanban's project filter, Memory's selected project, etc.) is the
 * responsibility of the page itself. This decouples the Kanban/Roadmap data
 * scope from the global `selectedProjectId` used by Memory/Code Graph/Agents.
 */

import { useEffect, useRef } from "react";
import { sseStore, type SSEEvent, type SSEEventType } from "../stores/sseStore";
import { getServerBaseUrl } from "@/api/serverUrl";
import { initSSEEventHandlers } from "../stores/sseEventHandlers";
import { fetchKanbanSnapshot } from "@/api/server";
import { useProjects } from "@/stores/useProjectStore";
import { projectStore } from "@/stores/projectStore";
import { taskStore } from "@/stores/taskStore";
import { epicStore } from "@/stores/epicStore";
import { resetMcpClient } from "@/api/mcpClient";
import { useProviderGateStore } from "@/stores/providerGateStore";

const INITIAL_RECONNECT_DELAY = 1000;
const MAX_RECONNECT_DELAY = 30000;
const RECONNECT_MULTIPLIER = 2;

export function useEventSource() {
  const projects = useProjects();
  // Re-run the effect when the *set* of projects changes (add/remove), not on
  // every metadata edit. zustand returns a new array reference whenever any
  // project field updates, so we key on a stable primitive instead.
  const projectIdsKey = projects
    .map((p) => p.id)
    .sort()
    .join(",");
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
    if (normalized === "proposal_created") return "proposal_created";
    if (normalized === "proposal_updated") return "proposal_updated";
    if (normalized === "proposal_deleted") return "proposal_deleted";
    if (normalized === "proposal_feedback_created") return "proposal_feedback_created";
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

    const hydrateSnapshot = async () => {
      const slugs = projectStore
        .getState()
        .projects.map((p) => `${p.github_owner}/${p.github_repo}`);
      if (slugs.length === 0) {
        // Projects haven't loaded yet — projectIdsKey will change once they
        // do, re-running this effect.
        return;
      }
      try {
        const snapshot = await fetchKanbanSnapshot(null, slugs);
        if (!isActive) return;
        taskStore.getState().setTasks(snapshot.tasks);
        epicStore.getState().setEpics(snapshot.epics);
      } catch (error) {
        console.error("Failed to hydrate Kanban snapshot:", error);
      }
    };

    // Initialize SSE event handlers (wire stores to SSE events)
    cleanupHandlersRef.current = initSSEEventHandlers();

    const connect = async () => {
      try {
        await hydrateSnapshot();

        // Build URL with Last-Event-ID if available. SSE is unfiltered —
        // the Kanban needs live updates across all projects.
        let url = `${getServerBaseUrl()}/events`;
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
          "proposal_created",
          "proposal_updated",
          "proposal_deleted",
          "proposal_feedback_created",
          "proposal.created",
          "proposal.updated",
          "proposal.deleted",
          "proposal_feedback.created",
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

        // Copilot's in-process OAuth still needs the browser-popup
        // fallback (its MCP handler opens the authorize URL on the
        // server side). Codex moved to the device-code flow — the
        // server emits `oauth.device_code` instead; the ChatGPT sign-in
        // card consumes it directly from the `provider_oauth_start`
        // response, so we no longer need a global popup handler.
        // Credential writes (vault upsert + OAuth token persistence) fire
        // `credential.updated`; re-check the provider gate so the onboarding
        // screen unmounts as soon as a new provider becomes usable — e.g.
        // right after the Codex device-code flow lands tokens in the DB.
        const refreshGate = () => {
          if (!isActive) return;
          void useProviderGateStore.getState().refresh();
        };
        es.addEventListener("credential.updated", refreshGate);
        es.addEventListener("credential.created", refreshGate);
        es.addEventListener("credential.deleted", refreshGate);

        es.addEventListener("oauth.open_browser", (event) => {
          if (!isActive) return;
          try {
            const envelope = JSON.parse(event.data);
            const url = envelope?.payload?.url;
            if (typeof url !== "string" || !url) return;
            const win = window.open(url, "_blank", "noopener,noreferrer");
            if (!win) {
              sseStore.getState().setError(
                new Error(
                  "Browser blocked the OAuth popup. Open this URL manually: " + url,
                ),
              );
              console.warn("oauth.open_browser: popup blocked; url:", url);
            }
          } catch (err) {
            console.error("Failed to handle oauth.open_browser:", err);
          }
        });

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
          sseStore.getState().setConnectionStatus("reconnecting");

          es.close();
          eventSourceRef.current = null;

          const { reconnectAttempt } = sseStore.getState();
          const delay = Math.min(
            INITIAL_RECONNECT_DELAY * Math.pow(RECONNECT_MULTIPLIER, reconnectAttempt),
            MAX_RECONNECT_DELAY
          );
          sseStore.getState().incrementReconnectAttempt();

          reconnectTimerRef.current = setTimeout(async () => {
            if (!isActive) return;
            // Reset MCP client so the next tool call reconnects cleanly.
            try {
              await resetMcpClient();
            } catch {
              // ignore — connect() below will surface any failure
            }
            connect();
          }, delay);
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
  }, [projectIdsKey]);

  return {
    eventSource: eventSourceRef.current,
  };
}
