import type { SSEEventType } from "./sseStore";

/**
 * Raw browser-visible SSE event names emitted by the server.
 *
 * Envelope-backed names are produced by `server/src/sse.rs::event_name` as
 * `${entity_type}.${action}`. `lagged` is emitted when the server-side broadcast
 * receiver falls behind, and `ping` is a named liveness event emitted outside the
 * envelope stream.
 */
export const SERVER_SSE_EVENT_NAMES = [
  "activity.logged",
  "agent.created",
  "agent.updated",
  "agent.deleted",
  "credential.created",
  "credential.updated",
  "credential.deleted",
  "credential.revoked",
  "custom_provider.updated",
  "custom_provider.deleted",
  "dispatch_pause.changed",
  "epic.created",
  "epic.updated",
  "epic.deleted",
  "git_settings.updated",
  "image.ready",
  "image.build_failed",
  "image.updated",
  "lifecycle.step",
  "note.created",
  "note.updated",
  "note.deleted",
  "note.contradiction_candidates",
  "note.missing_summary",
  "oauth.open_browser",
  "oauth.device_code",
  "project.created",
  "project.updated",
  "project.deleted",
  "project.health_ok",
  "project.health_error",
  "project_image.ready",
  "project_image.build_failed",
  "project_image.updated",
  "project_config.updated",
  "proposal.created",
  "proposal.updated",
  "proposal.deleted",
  "proposal_feedback.created",
  "session.dispatched",
  "session.started",
  "session.completed",
  "session.interrupted",
  "session.failed",
  "session.ended",
  "session.updated",
  "session.message",
  "session_message.inserted",
  "session.token_update",
  "setting.updated",
  "sync.completed",
  "task.created",
  "task.updated",
  "task.deleted",
  "lagged",
  "ping",
] as const;

export type ServerSSEEventName = (typeof SERVER_SSE_EVENT_NAMES)[number];

export type ServerSSEEventDecision =
  | { kind: "dispatch"; eventType: SSEEventType }
  | { kind: "hydrate"; reason: "lagged" }
  | { kind: "liveness"; reason: "ping" }
  | { kind: "hook"; reason: "oauth_open_browser" }
  | { kind: "ignore"; reason: string };

type ServerSSEEventDecisionMap = {
  readonly [Name in ServerSSEEventName]: ServerSSEEventDecision;
};

/**
 * Exhaustive decision table for every known server SSE name.
 *
 * The `satisfies` clause is intentional: adding a value to
 * `SERVER_SSE_EVENT_NAMES` without choosing dispatch vs explicit no-dispatch
 * fails TypeScript.
 */
export const SERVER_SSE_EVENT_DECISIONS = {
  "activity.logged": { kind: "ignore", reason: "activity feeds are refetched by their views" },
  "agent.created": { kind: "dispatch", eventType: "project_changed" },
  "agent.updated": { kind: "dispatch", eventType: "project_changed" },
  "agent.deleted": { kind: "dispatch", eventType: "project_changed" },
  "credential.created": { kind: "dispatch", eventType: "credential_created" },
  "credential.updated": { kind: "dispatch", eventType: "credential_updated" },
  "credential.deleted": { kind: "dispatch", eventType: "credential_deleted" },
  "credential.revoked": { kind: "dispatch", eventType: "credential_revoked" },
  "custom_provider.updated": { kind: "dispatch", eventType: "project_changed" },
  "custom_provider.deleted": { kind: "dispatch", eventType: "project_changed" },
  "dispatch_pause.changed": { kind: "dispatch", eventType: "dispatch_pause_changed" },
  "epic.created": { kind: "dispatch", eventType: "epic_created" },
  "epic.updated": { kind: "dispatch", eventType: "epic_updated" },
  "epic.deleted": { kind: "dispatch", eventType: "epic_deleted" },
  "git_settings.updated": { kind: "dispatch", eventType: "project_changed" },
  "image.ready": { kind: "dispatch", eventType: "project_changed" },
  "image.build_failed": { kind: "dispatch", eventType: "project_changed" },
  "image.updated": { kind: "dispatch", eventType: "project_changed" },
  "lifecycle.step": { kind: "dispatch", eventType: "lifecycle_step" },
  "note.created": { kind: "ignore", reason: "memory views load notes through MCP queries" },
  "note.updated": { kind: "ignore", reason: "memory views load notes through MCP queries" },
  "note.deleted": { kind: "ignore", reason: "memory views load notes through MCP queries" },
  "note.contradiction_candidates": {
    kind: "ignore",
    reason: "contradiction follow-up is handled by the memory control-plane worker",
  },
  "note.missing_summary": {
    kind: "ignore",
    reason: "summary backfill is handled by the memory control-plane worker",
  },
  "oauth.open_browser": { kind: "hook", reason: "oauth_open_browser" },
  "oauth.device_code": { kind: "ignore", reason: "device-code responses are consumed by the initiating flow" },
  "project.created": { kind: "dispatch", eventType: "project_changed" },
  "project.updated": { kind: "dispatch", eventType: "project_changed" },
  "project.deleted": { kind: "dispatch", eventType: "project_changed" },
  "project.health_ok": { kind: "dispatch", eventType: "project_changed" },
  "project.health_error": { kind: "dispatch", eventType: "project_changed" },
  "project_image.ready": { kind: "dispatch", eventType: "project_changed" },
  "project_image.build_failed": { kind: "dispatch", eventType: "project_changed" },
  "project_image.updated": { kind: "dispatch", eventType: "project_changed" },
  "project_config.updated": { kind: "dispatch", eventType: "project_changed" },
  "proposal.created": { kind: "dispatch", eventType: "proposal_created" },
  "proposal.updated": { kind: "dispatch", eventType: "proposal_updated" },
  "proposal.deleted": { kind: "dispatch", eventType: "proposal_deleted" },
  "proposal_feedback.created": { kind: "dispatch", eventType: "proposal_feedback_created" },
  "session.dispatched": { kind: "dispatch", eventType: "session_dispatched" },
  "session.started": { kind: "dispatch", eventType: "session_started" },
  "session.completed": { kind: "dispatch", eventType: "session_ended" },
  "session.interrupted": { kind: "dispatch", eventType: "session_ended" },
  "session.failed": { kind: "dispatch", eventType: "session_ended" },
  "session.ended": { kind: "dispatch", eventType: "session_ended" },
  "session.updated": { kind: "dispatch", eventType: "session_ended" },
  "session.message": { kind: "dispatch", eventType: "session_message" },
  "session_message.inserted": {
    kind: "ignore",
    reason: "legacy chat-message repository event; live task session messages use session.message",
  },
  "session.token_update": { kind: "ignore", reason: "token counters are loaded through session detail queries" },
  "setting.updated": { kind: "dispatch", eventType: "project_changed" },
  "sync.completed": { kind: "dispatch", eventType: "sync_completed" },
  "task.created": { kind: "dispatch", eventType: "task_created" },
  "task.updated": { kind: "dispatch", eventType: "task_updated" },
  "task.deleted": { kind: "dispatch", eventType: "task_deleted" },
  lagged: { kind: "hydrate", reason: "lagged" },
  ping: { kind: "liveness", reason: "ping" },
} as const satisfies ServerSSEEventDecisionMap;

const warnedUnknownEventNames = new Set<string>();

export function isServerSSEEventName(rawName: string): rawName is ServerSSEEventName {
  return Object.prototype.hasOwnProperty.call(SERVER_SSE_EVENT_DECISIONS, rawName);
}

export function resolveServerSSEEventName(
  rawName: string,
): ServerSSEEventDecision | { kind: "unknown" } {
  if (isServerSSEEventName(rawName)) {
    return SERVER_SSE_EVENT_DECISIONS[rawName];
  }

  if (!warnedUnknownEventNames.has(rawName)) {
    warnedUnknownEventNames.add(rawName);
    console.warn(`[SSE] Unknown server event name: ${rawName}`);
  }

  return { kind: "unknown" };
}

export function resetUnknownSSEEventWarningsForTest(): void {
  warnedUnknownEventNames.clear();
}
