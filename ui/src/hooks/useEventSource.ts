/**
 * useEventSource hook - Manages EventSource connection with auto-reconnect
 *
 * Features:
 * - Stores EventSource in useRef to prevent re-renders
 * - Connects to http://127.0.0.1:{port}/events on startup
 * - Exponential backoff with jitter on connection errors
 * - Treats any registered SSE event or ping as stream liveness
 * - Tracks Last-Event-ID for replay on reconnect
 * - Manages connection status: connected | reconnecting | error
 *
 * Task/epic data is hydrated for ALL projects unconditionally. Per-page
 * filtering (Kanban's project filter, Memory's selected project, etc.) is the
 * responsibility of the page itself. This decouples the Kanban/Roadmap data
 * scope from the global `selectedProjectId` used by Memory/Code Graph/Agents.
 */

import { useEffect, useRef } from "react";
import { sseStore, type SSEEvent } from "../stores/sseStore";
import { getServerBaseUrl } from "@/api/serverUrl";
import { initSSEEventHandlers } from "../stores/sseEventHandlers";
import { fetchKanbanSnapshot } from "@/api/server";
import { useProjects } from "@/stores/useProjectStore";
import { projectStore } from "@/stores/projectStore";
import { taskStore } from "@/stores/taskStore";
import { epicStore } from "@/stores/epicStore";
import { resetMcpClient } from "@/api/mcpClient";
import { useProviderGateStore } from "@/stores/providerGateStore";
import { refreshDispatchPauseStatus } from "@/stores/dispatchPauseStore";
import {
  SERVER_SSE_EVENT_DECISIONS,
  SERVER_SSE_EVENT_NAMES,
  type ServerSSEEventName,
} from "@/stores/sseEventContract";

export const INITIAL_RECONNECT_DELAY = 1000;
export const MAX_RECONNECT_DELAY = 30000;
export const RECONNECT_MULTIPLIER = 2;
export const RECONNECT_JITTER_RATIO = 0.2;
export const SILENCE_TIMEOUT_MS = 60_000;
export const LIVENESS_CHECK_INTERVAL_MS = 1_000;

export function calculateReconnectDelay(
  reconnectAttempt: number,
  random = Math.random,
): number {
  const baseDelay = Math.min(
    INITIAL_RECONNECT_DELAY * Math.pow(RECONNECT_MULTIPLIER, reconnectAttempt),
    MAX_RECONNECT_DELAY,
  );
  const minDelay = Math.max(0, baseDelay * (1 - RECONNECT_JITTER_RATIO));
  const maxDelay = Math.min(MAX_RECONNECT_DELAY, baseDelay * (1 + RECONNECT_JITTER_RATIO));
  return Math.round(minDelay + random() * (maxDelay - minDelay));
}

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
  const livenessTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const cleanupHandlersRef = useRef<(() => void) | null>(null);
  const lastReceivedAtRef = useRef<number>(Date.now());

  useEffect(() => {
    let isActive = true;
    let connectGeneration = 0;

    const markReceived = () => {
      lastReceivedAtRef.current = Date.now();
    };

    const clearReconnectTimer = () => {
      if (reconnectTimerRef.current) {
        clearTimeout(reconnectTimerRef.current);
        reconnectTimerRef.current = null;
      }
    };

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

    const hydratePauseStatus = async () => {
      try {
        await refreshDispatchPauseStatus();
      } catch (error) {
        console.error("Failed to hydrate dispatch pause status:", error);
      }
    };

    const scheduleReconnect = (source: EventSource | null) => {
      if (!isActive) return;

      clearReconnectTimer();
      connectGeneration += 1;

      sseStore.getState().setConnected(false);
      sseStore.getState().setConnectionStatus("reconnecting");

      const currentSource = source ?? eventSourceRef.current;
      if (currentSource) {
        currentSource.close();
      }
      if (!source || eventSourceRef.current === source) {
        eventSourceRef.current = null;
      }

      const { reconnectAttempt } = sseStore.getState();
      const delay = calculateReconnectDelay(reconnectAttempt);
      sseStore.getState().incrementReconnectAttempt();

      reconnectTimerRef.current = setTimeout(async () => {
        if (!isActive) return;
        // Reset MCP client so the next tool call reconnects cleanly.
        try {
          await resetMcpClient();
        } catch {
          // ignore — connect() below will surface any failure
        }
        void connect();
      }, delay);
    };

    const handleOAuthOpenBrowser = (event: MessageEvent) => {
      markReceived();
      if (!isActive) return;
      try {
        const envelope = JSON.parse(event.data);
        const url = envelope?.payload?.url;
        if (typeof url !== "string" || !url) return;
        const win = window.open(url, "_blank", "noopener,noreferrer");
        if (!win) {
          sseStore.getState().setError(
            new Error("Browser blocked the OAuth popup. Open this URL manually: " + url),
          );
          console.warn("oauth.open_browser: popup blocked; url:", url);
        }
      } catch (err) {
        console.error("Failed to handle oauth.open_browser:", err);
      }
    };

    // Initialize SSE event handlers (wire stores to SSE events)
    cleanupHandlersRef.current = initSSEEventHandlers();

    const connect = async () => {
      const generation = connectGeneration;
      try {
        await hydrateSnapshot();

        // Build URL with Last-Event-ID if available. SSE is unfiltered —
        // the Kanban needs live updates across all projects.
        let url = `${getServerBaseUrl()}/events`;
        const lastEventId = sseStore.getState().lastEventId;
        if (lastEventId) {
          url += `?lastEventId=${encodeURIComponent(lastEventId)}`;
        }

        if (!isActive || generation !== connectGeneration) return;

        const es = new EventSource(url);
        eventSourceRef.current = es;

        es.onopen = () => {
          markReceived();
          if (!isActive || eventSourceRef.current !== es) return;
          if (sseStore.getState().reconnectAttempt > 0) {
            void hydrateSnapshot();
            void hydratePauseStatus();
          }
          sseStore.getState().resetReconnectAttempt();
          sseStore.getState().setConnected(true);
          sseStore.getState().setConnectionStatus("connected");
          sseStore.getState().setError(null);
        };

        // Copilot's in-process OAuth still needs the browser-popup
        // fallback (its MCP handler opens the authorize URL on the
        // server side). Codex moved to the device-code flow — the
        // server emits `oauth.device_code` instead; the ChatGPT sign-in
        // card consumes it directly from the `provider_oauth_start`
        // response, so we no longer need a global popup handler.
        es.addEventListener("oauth.open_browser", handleOAuthOpenBrowser);

        SERVER_SSE_EVENT_NAMES.forEach((eventType: ServerSSEEventName) => {
          if (eventType === "oauth.open_browser") return;

          es.addEventListener(eventType, (event) => {
            markReceived();
            if (!isActive || eventSourceRef.current !== es) return;

            const decision = SERVER_SSE_EVENT_DECISIONS[eventType];

            try {
              if (decision.kind === "liveness") {
                return;
              }

              if (decision.kind === "hydrate") {
                void hydrateSnapshot();
                void hydratePauseStatus();
                return;
              }

              if (decision.kind === "hook") {
                return;
              }

              const data = JSON.parse(event.data);

              // Track the event ID from the SSE message if present
              const eventId = (event as MessageEvent).lastEventId || undefined;
              if (eventId) {
                sseStore.getState().setLastEventId(eventId);
              }

              const sseEvent: SSEEvent = {
                type: decision.eventType,
                data,
                timestamp: Date.now(),
                id: eventId,
              };
              sseStore.getState().emit(sseEvent);

              if (eventType.startsWith("credential.")) {
                void useProviderGateStore.getState().refresh();
              }
            } catch (err) {
              console.error(`Failed to parse ${eventType} event:`, err);
            }
          });
        });

        es.onerror = () => {
          if (!isActive || eventSourceRef.current !== es) return;
          scheduleReconnect(es);
        };
      } catch (err) {
        if (!isActive) return;
        console.error("Failed to connect to EventSource:", err);
        sseStore.getState().setConnectionStatus("error");
        sseStore.getState().setError(err instanceof Error ? err : new Error(String(err)));
      }
    };

    livenessTimerRef.current = setInterval(() => {
      if (!isActive || !eventSourceRef.current) return;
      if (Date.now() - lastReceivedAtRef.current >= SILENCE_TIMEOUT_MS) {
        scheduleReconnect(eventSourceRef.current);
      }
    }, LIVENESS_CHECK_INTERVAL_MS);

    void connect();

    return () => {
      isActive = false;
      connectGeneration += 1;
      clearReconnectTimer();
      if (livenessTimerRef.current) {
        clearInterval(livenessTimerRef.current);
        livenessTimerRef.current = null;
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
