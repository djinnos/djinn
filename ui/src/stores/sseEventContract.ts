import type { SSEEventType } from "./sseStore";

export const SERVER_SSE_EVENT_NAMES = [
  "lagged",
  "ping",
  "task.created",
  "task.updated",
  "task.deleted",
  "epic.created",
  "epic.updated",
  "epic.deleted",
  "proposal.created",
  "proposal.updated",
  "proposal.deleted",
  "proposal_feedback.created",
  "project.created",
  "project.updated",
  "project.deleted",
  "project.changed",
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
  "lifecycle.step",
  "dispatch_pause.changed",
  "credential.created",
  "credential.updated",
  "credential.deleted",
  "credential.revoked",
  "oauth.open_browser",
] as const;

export type ServerSSEEventName = (typeof SERVER_SSE_EVENT_NAMES)[number];

export type ServerSSEEventDecision =
  | { kind: "dispatch"; eventType: SSEEventType }
  | { kind: "hydrate" }
  | { kind: "liveness" }
  | { kind: "hook" };

type ServerSSEEventDecisionMap = Record<ServerSSEEventName, ServerSSEEventDecision>;

export const SERVER_SSE_EVENT_DECISIONS = {
  lagged: { kind: "hydrate" },
  ping: { kind: "liveness" },
  "task.created": { kind: "dispatch", eventType: "task_created" },
  "task.updated": { kind: "dispatch", eventType: "task_updated" },
  "task.deleted": { kind: "dispatch", eventType: "task_deleted" },
  "epic.created": { kind: "dispatch", eventType: "epic_created" },
  "epic.updated": { kind: "dispatch", eventType: "epic_updated" },
  "epic.deleted": { kind: "dispatch", eventType: "epic_deleted" },
  "proposal.created": { kind: "dispatch", eventType: "proposal_created" },
  "proposal.updated": { kind: "dispatch", eventType: "proposal_updated" },
  "proposal.deleted": { kind: "dispatch", eventType: "proposal_deleted" },
  "proposal_feedback.created": { kind: "dispatch", eventType: "proposal_feedback_created" },
  "project.created": { kind: "dispatch", eventType: "project_changed" },
  "project.updated": { kind: "dispatch", eventType: "project_changed" },
  "project.deleted": { kind: "dispatch", eventType: "project_changed" },
  "project.changed": { kind: "dispatch", eventType: "project_changed" },
  "project.health_ok": { kind: "dispatch", eventType: "project_changed" },
  "project.health_error": { kind: "dispatch", eventType: "project_changed" },
  "session.message": { kind: "dispatch", eventType: "session_message" },
  "session.dispatched": { kind: "dispatch", eventType: "session_dispatched" },
  "session.started": { kind: "dispatch", eventType: "session_started" },
  "session.completed": { kind: "dispatch", eventType: "session_ended" },
  "session.interrupted": { kind: "dispatch", eventType: "session_ended" },
  "session.failed": { kind: "dispatch", eventType: "session_ended" },
  "session.updated": { kind: "dispatch", eventType: "session_ended" },
  "sync.completed": { kind: "dispatch", eventType: "sync_completed" },
  "verification.step": { kind: "dispatch", eventType: "verification_step" },
  "lifecycle.step": { kind: "dispatch", eventType: "lifecycle_step" },
  "dispatch_pause.changed": { kind: "dispatch", eventType: "dispatch_pause_changed" },
  "credential.created": { kind: "dispatch", eventType: "credential_created" },
  "credential.updated": { kind: "dispatch", eventType: "credential_updated" },
  "credential.deleted": { kind: "dispatch", eventType: "credential_deleted" },
  "credential.revoked": { kind: "dispatch", eventType: "credential_revoked" },
  "oauth.open_browser": { kind: "hook" },
} as const satisfies ServerSSEEventDecisionMap;

const warnedUnknownNames = new Set<string>();

export function resolveServerSSEEventName(rawName: string): ServerSSEEventDecision | null {
  if (Object.prototype.hasOwnProperty.call(SERVER_SSE_EVENT_DECISIONS, rawName)) {
    return SERVER_SSE_EVENT_DECISIONS[rawName as ServerSSEEventName];
  }

  if (!warnedUnknownNames.has(rawName)) {
    warnedUnknownNames.add(rawName);
    console.warn(`[SSE] Unknown server event name: ${rawName}`);
  }

  return null;
}

export function resetUnknownSSEEventWarningsForTest(): void {
  warnedUnknownNames.clear();
}
