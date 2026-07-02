/**
 * SSE Event Handlers - Wire SSE events to task/epic stores
 *
 * Sets up subscriptions to SSE events and updates stores directly
 * from full-entity event payloads. No mapping needed — types match MCP wire format.
 */

import { sseStore, type SSEEvent } from "./sseStore";
import { taskStore } from "./taskStore";
import { epicStore } from "./epicStore";
import { proposalStore } from "./proposalStore";
import { projectStore } from "./projectStore";
import {
  debounceInvalidateQueries,
  flushDebouncedInvalidations,
  queryClient,
} from "@/lib/queryClient";
import { fetchProjects } from "@/api/server";
import { showToast } from "@/lib/toast";
import type { Task, Epic, Proposal } from "@/api/types";
import { applyDispatchPauseSsePayload, type DispatchPauseSsePayload } from "./dispatchPauseStore";

/**
 * Unwrap SSE event payload.
 *
 * The server sends DjinnEventEnvelope format:
 *   {"entity_type":"task","action":"created","payload":{"task":{...},"from_sync":false}}
 *
 * For entity events (task/epic), the payload nests the entity under a key
 * matching the entity_type (e.g. payload.task). This helper extracts the
 * inner entity so callers receive the flat entity object.
 */
function unwrapPayload(raw: unknown): Record<string, unknown> {
  const obj = raw as Record<string, unknown>;
  if (!obj || typeof obj !== "object") return obj;

  // DjinnEventEnvelope format: extract payload field
  if ("entity_type" in obj && "payload" in obj && typeof obj.payload === "object" && obj.payload !== null) {
    const payload = obj.payload as Record<string, unknown>;
    const entityType = obj.entity_type as string;

    // Entity events nest under entity_type key, e.g. payload.task for task events
    if (entityType in payload) {
      const entity = payload[entityType];
      if (entity && typeof entity === "object") {
        return entity as Record<string, unknown>;
      }
    }

    // Non-entity events (session, sync, etc.) — return payload directly
    return payload;
  }

  // Legacy format: {data: {...entity...}}
  if ("data" in obj && typeof obj.data === "object") {
    return obj.data as Record<string, unknown>;
  }

  return obj;
}

/**
 * SSE sends some array fields as JSON strings (e.g. labels, acceptance_criteria).
 * Parse them back to arrays before storing.
 */
function normalizeSSEPayload(payload: Record<string, unknown>): Record<string, unknown> {
  const result = { ...payload };
  for (const key of ["labels", "acceptance_criteria", "memory_refs"]) {
    if (typeof result[key] === "string") {
      try {
        result[key] = JSON.parse(result[key] as string);
      } catch {
        // leave as-is
      }
    }
  }
  return result;
}

// Track subscription cleanup functions
let taskCreatedUnsub: (() => void) | null = null;
let taskUpdatedUnsub: (() => void) | null = null;
let taskDeletedUnsub: (() => void) | null = null;
let epicCreatedUnsub: (() => void) | null = null;
let epicUpdatedUnsub: (() => void) | null = null;
let epicDeletedUnsub: (() => void) | null = null;
let proposalCreatedUnsub: (() => void) | null = null;
let proposalUpdatedUnsub: (() => void) | null = null;
let proposalDeletedUnsub: (() => void) | null = null;
let proposalFeedbackUnsub: (() => void) | null = null;
let dispatchPauseChangedUnsub: (() => void) | null = null;

/**
 * Initialize SSE event handlers
 * Call this once at app startup to wire SSE events to stores
 */
export function initSSEEventHandlers(): () => void {
  const { subscribe } = sseStore.getState();

  // Task events — SSE sends snake_case MCP payloads wrapped in {type,action,data}.
  // The store holds ALL projects' tasks unconditionally; per-page filtering
  // (Kanban's URL project filter, Memory's selected project, etc.) is the
  // responsibility of each page. Filtering here by the global selectedProjectId
  // would silently starve the Kanban/Roadmap of live updates for any project
  // other than the one picked on the Memory/Code Graph picker.
  taskCreatedUnsub = subscribe("task_created", (event: SSEEvent) => {
    const task = normalizeSSEPayload(unwrapPayload(event.data)) as unknown as Task;
    if (!task.id || !task.title) {
      console.warn("[SSE] task_created with missing id/title, skipping:", task);
      return;
    }
    taskStore.getState().addTask(task);
    queryClient.setQueryData(["tasks"], (current: Task[] | undefined) =>
      current ? [...current, task] : [task]
    );
  });

  dispatchPauseChangedUnsub = subscribe("dispatch_pause_changed", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data) as DispatchPauseSsePayload;
    applyDispatchPauseSsePayload(payload);
  });

  taskUpdatedUnsub = subscribe("task_updated", (event: SSEEvent) => {
    const task = normalizeSSEPayload(unwrapPayload(event.data)) as unknown as Task;
    if (!task.id) return;

    // SSE task.updated payloads don't include active_session or session_count
    // (those are only added by MCP task_list/task_show). Preserve the values
    // that the session_started handler already set on the store — but only
    // for in-flight statuses. If the task moved back to open/closed, clear
    // the session so stale avatars don't linger.
    const IN_FLIGHT = new Set([
      "in_progress", "needs_task_review",
      "in_task_review", "needs_lead_intervention", "in_lead_intervention",
    ]);
    const existing = taskStore.getState().getTask(task.id);
    if (!existing) {
      // Don't create tasks from update events — wait for a full task_created or snapshot
      console.warn("[SSE] task_updated for unknown task, skipping:", task.id);
      return;
    }

    const isInFlight = IN_FLIGHT.has(task.status);
    if (!("active_session" in task)) {
      task.active_session = isInFlight ? existing.active_session : undefined;
    }
    if (!("session_count" in task)) task.session_count = existing.session_count;
    if (!("duration_seconds" in task)) task.duration_seconds = existing.duration_seconds;

    taskStore.getState().updateTask(task);
    queryClient.setQueryData(["tasks"], (current: Task[] | undefined) =>
      current?.map((t) => (t.id === task.id ? task : t))
    );
  });

  taskDeletedUnsub = subscribe("task_deleted", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data) as { id: string };
    taskStore.getState().removeTask(payload.id);
    queryClient.setQueryData(["tasks"], (current: { id: string }[] | undefined) =>
      current?.filter((task) => task.id !== payload.id)
    );
  });

  // Epic events — SSE sends snake_case MCP payloads wrapped in {type,action,data}
  epicCreatedUnsub = subscribe("epic_created", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data);
    const epic = payload as unknown as Epic;
    epicStore.getState().addEpic(epic);
    queryClient.setQueryData(["epics"], (current: Epic[] | undefined) =>
      current ? [...current, epic] : [epic]
    );
  });

  epicUpdatedUnsub = subscribe("epic_updated", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data);
    const epic = payload as unknown as Epic;
    epicStore.getState().updateEpic(epic);
    queryClient.setQueryData(["epics"], (current: Epic[] | undefined) =>
      current?.map((e) => (e.id === epic.id ? epic : e))
    );
  });

  epicDeletedUnsub = subscribe("epic_deleted", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data) as { id: string };
    epicStore.getState().removeEpic(payload.id);
    queryClient.setQueryData(["epics"], (current: { id: string }[] | undefined) =>
      current?.filter((epic) => epic.id !== payload.id)
    );
  });

  // Proposal events — global (no project scope). The SSE payload is the raw
  // `Proposal` model whose `acceptance_criteria` is a JSON string; normalize it
  // to an array before storing. Queries (["proposals", ...]) are invalidated so
  // the list + open detail re-fetch.
  proposalCreatedUnsub = subscribe("proposal_created", (event: SSEEvent) => {
    const proposal = normalizeSSEPayload(unwrapPayload(event.data)) as unknown as Proposal;
    if (!proposal.id) return;
    proposalStore.getState().addProposal(proposal);
    debounceInvalidateQueries({ queryKey: ["proposals"] });
  });

  proposalUpdatedUnsub = subscribe("proposal_updated", (event: SSEEvent) => {
    const proposal = normalizeSSEPayload(unwrapPayload(event.data)) as unknown as Proposal;
    if (!proposal.id) return;
    proposalStore.getState().updateProposal(proposal);
    debounceInvalidateQueries({ queryKey: ["proposals"] });
  });

  proposalDeletedUnsub = subscribe("proposal_deleted", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data) as { id: string };
    proposalStore.getState().removeProposal(payload.id);
    debounceInvalidateQueries({ queryKey: ["proposals"] });
  });

  proposalFeedbackUnsub = subscribe("proposal_feedback_created", () => {
    // Feedback changes only affect a proposal's detail view; re-fetch it.
    debounceInvalidateQueries({ queryKey: ["proposals"] });
  });

  // Session events — update active_session on the corresponding task
  const sessionDispatchedUnsub = subscribe("session_dispatched", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data) as {
      task_id?: string;
      agent_type?: string;
      model_id?: string;
    };
    if (!payload.task_id) return;
    const existing = taskStore.getState().getTask(payload.task_id);
    if (!existing) return;
    taskStore.getState().updateTask({
      ...existing,
      active_session: {
        session_id: undefined,
        agent_type: payload.agent_type,
        model_id: payload.model_id,
        started_at: new Date().toISOString(),
        // Live pre-session tracking state (matches the backend
        // `TaskRunStatus::Starting` wire string and what `task_show`/
        // `task_list` surface on refetch) so the card renders a real
        // "starting" status instead of the old derived "setting up" label.
        status: "starting",
      },
    });
  });

  const sessionStartedUnsub = subscribe("session_started", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data) as {
      id?: string;
      task_id?: string;
      agent_type?: string;
      model_id?: string;
      started_at?: string;
      status?: string;
    };
    if (!payload.task_id) return;
    const existing = taskStore.getState().getTask(payload.task_id);
    if (!existing) return;
    taskStore.getState().updateTask({
      ...existing,
      active_session: {
        session_id: payload.id,
        agent_type: payload.agent_type,
        model_id: payload.model_id,
        started_at: payload.started_at,
        status: payload.status,
      },
    });
  });

  const sessionEndedUnsub = subscribe("session_ended", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data) as { task_id?: string };
    if (!payload.task_id) return;
    const existing = taskStore.getState().getTask(payload.task_id);
    if (!existing) return;
    taskStore.getState().updateTask({
      ...existing,
      active_session: undefined,
      session_count: (existing.session_count ?? 0) + 1,
    });
  });

  // Sync events — when an import brings in new tasks, the individual task.updated
  // SSE events (from_sync=true) will have already updated the stores. This handler
  // is for visibility — invalidate queries so any list views re-fetch.
  const syncCompletedUnsub = subscribe("sync_completed", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data) as {
      channel?: string;
      direction?: string;
      count?: number;
      error?: string | null;
    };
    // Only refresh on successful imports that actually changed data.
    if (payload.direction === "import" && (payload.count ?? 0) > 0) {
      debounceInvalidateQueries({ queryKey: ["tasks"] });
      debounceInvalidateQueries({ queryKey: ["epics"] });
    }
  });

  const projectChangedUnsub = subscribe("project_changed", () => {
    debounceInvalidateQueries({ queryKey: ["providers"] });
    debounceInvalidateQueries({ queryKey: ["settings"] });
    fetchProjects()
      .then((projects) => projectStore.getState().setProjects(projects))
      .catch((err) => console.error("Failed to refetch projects after SSE event:", err));
  });

  // A stored credential was rejected (401) and marked revoked server-side —
  // surface an app-wide heads-up so the owner reconnects, even if they aren't on
  // the Settings page. The Settings card also reflects it (persisted, F5-safe).
  const credentialRevokedUnsub = subscribe("credential_revoked", (event: SSEEvent) => {
    const payload = unwrapPayload(event.data) as {
      provider_id?: string;
      reason?: string;
    };
    showToast.error("Provider disconnected", {
      description:
        payload.reason ??
        `${payload.provider_id ?? "A provider"} was disconnected. Reconnect it in Settings.`,
      duration: 10000,
    });
  });

  // Return cleanup function
  return () => {
    credentialRevokedUnsub?.();
    taskCreatedUnsub?.();
    taskUpdatedUnsub?.();
    taskDeletedUnsub?.();
    epicCreatedUnsub?.();
    epicUpdatedUnsub?.();
    epicDeletedUnsub?.();
    proposalCreatedUnsub?.();
    proposalUpdatedUnsub?.();
    proposalDeletedUnsub?.();
    proposalFeedbackUnsub?.();
    dispatchPauseChangedUnsub?.();
    projectChangedUnsub?.();
    sessionDispatchedUnsub?.();
    sessionStartedUnsub?.();
    sessionEndedUnsub?.();
    syncCompletedUnsub?.();
    flushDebouncedInvalidations();
  };
}

/**
 * Cleanup SSE event handlers
 */
export function cleanupSSEEventHandlers(): void {
  taskCreatedUnsub?.();
  taskUpdatedUnsub?.();
  taskDeletedUnsub?.();
  epicCreatedUnsub?.();
  epicUpdatedUnsub?.();
  epicDeletedUnsub?.();
  proposalCreatedUnsub?.();
  proposalUpdatedUnsub?.();
  proposalDeletedUnsub?.();
  proposalFeedbackUnsub?.();
  dispatchPauseChangedUnsub?.();

  taskCreatedUnsub = null;
  taskUpdatedUnsub = null;
  taskDeletedUnsub = null;
  epicCreatedUnsub = null;
  epicUpdatedUnsub = null;
  epicDeletedUnsub = null;
  proposalCreatedUnsub = null;
  proposalUpdatedUnsub = null;
  proposalDeletedUnsub = null;
  proposalFeedbackUnsub = null;
  dispatchPauseChangedUnsub = null;
  flushDebouncedInvalidations();
}
