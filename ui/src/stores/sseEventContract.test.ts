import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import {
  SERVER_SSE_EVENT_DECISIONS,
  SERVER_SSE_EVENT_NAMES,
  resetUnknownSSEEventWarningsForTest,
  resolveServerSSEEventName,
} from "./sseEventContract";

describe("sseEventContract", () => {
  beforeEach(() => {
    resetUnknownSSEEventWarningsForTest();
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("has an explicit mapping or non-dispatch decision for every server event name", () => {
    expect(Object.keys(SERVER_SSE_EVENT_DECISIONS).sort()).toEqual(
      [...SERVER_SSE_EVENT_NAMES].sort(),
    );
  });

  it("maps dispatchable raw dotted server names to UI SSE event types", () => {
    expect(resolveServerSSEEventName("task.created")).toEqual({
      kind: "dispatch",
      eventType: "task_created",
    });
    expect(resolveServerSSEEventName("proposal_feedback.created")).toEqual({
      kind: "dispatch",
      eventType: "proposal_feedback_created",
    });
    expect(resolveServerSSEEventName("proposal.created")).toEqual({
      kind: "dispatch",
      eventType: "proposal_created",
    });
    expect(resolveServerSSEEventName("proposal.updated")).toEqual({
      kind: "dispatch",
      eventType: "proposal_updated",
    });
    expect(resolveServerSSEEventName("proposal.deleted")).toEqual({
      kind: "dispatch",
      eventType: "proposal_deleted",
    });
    expect(resolveServerSSEEventName("session.completed")).toEqual({
      kind: "dispatch",
      eventType: "session_ended",
    });
    expect(resolveServerSSEEventName("dispatch_pause.changed")).toEqual({
      kind: "dispatch",
      eventType: "dispatch_pause_changed",
    });
    expect(resolveServerSSEEventName("credential.revoked")).toEqual({
      kind: "dispatch",
      eventType: "credential_revoked",
    });
    expect(resolveServerSSEEventName("project_image.ready")).toEqual({
      kind: "dispatch",
      eventType: "project_changed",
    });
  });

  it("treats lagged and ping as explicit non-dispatch decisions", () => {
    expect(resolveServerSSEEventName("lagged")).toEqual({ kind: "hydrate", reason: "lagged" });
    expect(resolveServerSSEEventName("ping")).toEqual({ kind: "liveness", reason: "ping" });
  });

  it("keeps legacy or worker-consumed envelope names explicit instead of dispatching them", () => {
    expect(resolveServerSSEEventName("session_message.inserted")).toMatchObject({
      kind: "ignore",
    });
    expect(resolveServerSSEEventName("note.contradiction_candidates")).toMatchObject({
      kind: "ignore",
    });
  });

  it("warns at most once per unknown raw event name", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);

    expect(resolveServerSSEEventName("task.archived")).toEqual({ kind: "unknown" });
    expect(resolveServerSSEEventName("task.archived")).toEqual({ kind: "unknown" });
    expect(resolveServerSSEEventName("proposal.merged")).toEqual({ kind: "unknown" });

    expect(warn).toHaveBeenCalledTimes(2);
    expect(warn).toHaveBeenNthCalledWith(1, "[SSE] Unknown server event name: task.archived");
    expect(warn).toHaveBeenNthCalledWith(2, "[SSE] Unknown server event name: proposal.merged");
  });
});
