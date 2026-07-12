 
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
import { sseStore, type SSEEvent } from "../stores/sseStore";
import { getServerBaseUrl } from "@/api/serverUrl";
import { initSSEEventHandlers } from "../stores/sseEventHandlers";
import { fetchActiveSnapshot, fetchClosedFirstPage } from "@/api/server";
import { useProjects } from "@/stores/useProjectStore";
import { projectStore } from "@/stores/projectStore";
import { taskStore } from "@/stores/taskStore";
import { epicStore } from "@/stores/epicStore";
import { resetMcpClient } from "@/api/mcpClient";
import { useProviderGateStore } from "@/stores/providerGateStore";
import { refreshDispatchPauseStatus } from "@/stores/dispatchPauseStore";
import {
  SERVER_SSE_EVENT_NAMES,
  resolveServerSSEEventName,
} from "@/stores/sseEventContract";

export const INITIAL_RECONNECT_DELAY = 1000;
export const MAX_RECONNECT_DELAY = 30000;
export const RECONNECT_MULTIPLIER = 2;
export const RECONNECT_JITTER_FACTOR = 0.2;
export const MIN_RECONNECT_DELAY = 1;
export const SILENCE_TIMEOUT_MS = 60_000;
export const LIVENESS_WATCHDOG_INTERVAL_MS = 5_000;

/**
 * Returns the capped exponential reconnect delay with symmetric bounded jitter.
 *
 * Jitter is applied within +/- RECONNECT_JITTER_FACTOR of the capped base delay,
 * then clamped so reconnects never happen below MIN_RECONNECT_DELAY or above
 * MAX_RECONNECT_DELAY.
 */
export function getReconnectDelay(
  reconnectAttempt: number,
  random = Math.random,
) {
  const baseDelay = Math.min(
    INITIAL_RECONNECT_DELAY * Math.pow(RECONNECT_MULTIPLIER, reconnectAttempt),
    MAX_RECONNECT_DELAY,
  );
  const jitterMultiplier = 1 + (random() * 2 - 1) * RECONNECT_JITTER_FACTOR;
  const jitteredDelay = baseDelay * jitterMultiplier;
  return Math.min(
    MAX_RECONNECT_DELAY,
    Math.max(MIN_RECONNECT_DELAY, jitteredDelay),
  );
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
  const lastReceivedAtRef = useRef<number>(Date.now());
  const cleanupHandlersRef = useRef<(() => void) | null>(null);
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
        // Phase 1 — active-first paint: fetch the non-closed tasks + epics and
        // render the Open / In Progress / PR Ready columns immediately, without
        // waiting on the (potentially thousands of) closed/merged rows.
        const active = await fetchActiveSnapshot(slugs);
        if (!isActive) return;
        taskStore.getState().setTasks(active.tasks);
        epicStore.getState().setEpics(active.epics);

        // Phase 2 — paginated Merged column: append the first page of closed
        // tasks. `upsertTasks` merges additively so the active columns painted
        // above are preserved. On reconnect/lag this resets the merged
        // pagination cursors and drops any previously-loaded extra pages.
        const closed = await fetchClosedFirstPage(slugs);
        if (!isActive) return;
        taskStore.getState().upsertTasks(closed);
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

    // Initialize SSE event handlers (wire stores to SSE events)
    cleanupHandlersRef.current = initSSEEventHandlers();

    const markServerEventReceived = () => {
      lastReceivedAtRef.current = Date.now();
    };

    const clearLivenessWatchdog = () => {
      if (livenessTimerRef.current) {
        clearInterval(livenessTimerRef.current);
        livenessTimerRef.current = null;
      }
    };

    let connect: () => Promise<void>;

    const scheduleReconnect = (source?: EventSource | null) => {
      if (!isActive) return;

      sseStore.getState().setConnected(false);
      sseStore.getState().setConnectionStatus("reconnecting");

      clearLivenessWatchdog();

      const activeEventSource = source ?? eventSourceRef.current;
      if (activeEventSource) {
        activeEventSource.close();
      }
      if (!source || eventSourceRef.current === source) {
        eventSourceRef.current = null;
      }

      if (reconnectTimerRef.current) {
        return;
      }

      const { reconnectAttempt } = sseStore.getState();
      const delay = getReconnectDelay(reconnectAttempt);
      sseStore.getState().incrementReconnectAttempt();

      reconnectTimerRef.current = setTimeout(async () => {
        reconnectTimerRef.current = null;
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

    const startLivenessWatchdog = () => {
      clearLivenessWatchdog();
      livenessTimerRef.current = setInterval(() => {
        if (!isActive || !eventSourceRef.current) return;
        if (Date.now() - lastReceivedAtRef.current < SILENCE_TIMEOUT_MS) return;
        scheduleReconnect(eventSourceRef.current);
      }, LIVENESS_WATCHDOG_INTERVAL_MS);
    };

    connect = async () => {
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
        markServerEventReceived();
        startLivenessWatchdog();

        es.onopen = () => {
          if (!isActive) return;
          markServerEventReceived();
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

        SERVER_SSE_EVENT_NAMES.forEach((eventType) => {
          es.addEventListener(eventType, (event) => {
            if (!isActive) return;
            markServerEventReceived();
            try {
              const decision = resolveServerSSEEventName(eventType);

              if (decision.kind === "hydrate") {
                void hydrateSnapshot();
                void hydratePauseStatus();
                return;
              }

              if (decision.kind !== "dispatch") {
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
            } catch (err) {
              console.error(`Failed to parse ${eventType} event:`, err);
            }
          });
        });

        es.onerror = () => {
          scheduleReconnect(es);
        };
      } catch (err) {
        if (!isActive) return;
        console.error("Failed to connect to EventSource:", err);
        sseStore.getState().setConnectionStatus("error");
        sseStore.getState().setError(err instanceof Error ? err : new Error(String(err)));
      }
    };

    void connect();

    return () => {
      isActive = false;
      if (reconnectTimerRef.current) {
        clearTimeout(reconnectTimerRef.current);
        reconnectTimerRef.current = null;
      }
      clearLivenessWatchdog();
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
