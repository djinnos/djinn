import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { projectStore } from "@/stores/projectStore";
import { sseStore, type SSEEvent } from "@/stores/sseStore";
import { resetMcpClient } from "@/api/mcpClient";
import { fetchActiveSnapshot, fetchClosedFirstPage } from "@/api/server";
import {
  getReconnectDelay,
  INITIAL_RECONNECT_DELAY,
  LIVENESS_WATCHDOG_INTERVAL_MS,
  MAX_RECONNECT_DELAY,
  MIN_RECONNECT_DELAY,
  RECONNECT_JITTER_FACTOR,
  RECONNECT_MULTIPLIER,
  SILENCE_TIMEOUT_MS,
  useEventSource,
} from "./useEventSource";

const { connectionActions } = vi.hoisted(() => ({
  connectionActions: [] as string[],
}));

vi.mock("@/api/serverUrl", () => ({
  getBaseUrl: () => "http://djinn.test",
  getServerBaseUrl: () => "http://djinn.test",
}));

vi.mock("@/api/server", () => ({
  fetchActiveSnapshot: vi.fn().mockResolvedValue({ tasks: [], epics: [] }),
  fetchClosedFirstPage: vi.fn().mockResolvedValue([]),
  fetchProjects: vi.fn().mockResolvedValue([]),
}));

vi.mock("@/api/mcpClient", () => ({
  resetMcpClient: vi.fn(async () => {
    connectionActions.push("reset-mcp");
  }),
}));

vi.mock("@/stores/dispatchPauseStore", () => ({
  applyDispatchPauseSsePayload: vi.fn(),
  refreshDispatchPauseStatus: vi.fn().mockResolvedValue(undefined),
}));

vi.mock("@/stores/providerGateStore", () => ({
  useProviderGateStore: {
    getState: () => ({ refresh: vi.fn().mockResolvedValue(undefined) }),
  },
}));

class MockEventSource {
  static instances: MockEventSource[] = [];

  readonly listeners = new Map<string, Set<(event: MessageEvent) => void>>();
  readonly close = vi.fn();
  onopen: (() => void) | null = null;
  onerror: (() => void) | null = null;

  constructor(readonly url: string) {
    MockEventSource.instances.push(this);
    connectionActions.push(`connect:${url}`);
  }

  addEventListener(type: string, handler: (event: MessageEvent) => void) {
    if (!this.listeners.has(type)) {
      this.listeners.set(type, new Set());
    }
    this.listeners.get(type)?.add(handler);
  }

  dispatch(type: string, data = "{}", lastEventId = "") {
    const event = { data, lastEventId } as MessageEvent;
    this.listeners.get(type)?.forEach((handler) => handler(event));
  }
}

function latestEventSource(): MockEventSource {
  const instance = MockEventSource.instances.at(-1);
  if (!instance) throw new Error("Expected an EventSource instance");
  return instance;
}

async function flushEffects() {
  await act(async () => {
    await Promise.resolve();
  });
}

async function advanceTimersBy(ms: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

async function mountEventSourceHook() {
  const hook = renderHook(() => useEventSource());
  await flushEffects();
  return hook;
}

function resetStores() {
  projectStore.getState().setProjects([
    {
      id: "project-1",
      name: "Project One",
      github_owner: "djinnos",
      github_repo: "djinn",
      created_at: "2026-01-01T00:00:00Z",
      updated_at: "2026-01-01T00:00:00Z",
    },
  ]);
  sseStore.setState({
    isConnected: false,
    connectionStatus: "reconnecting",
    lastError: null,
    reconnectAttempt: 0,
    lastEventId: null,
    handlers: new Map(),
  });
}

describe("getReconnectDelay", () => {
  it("applies bounded symmetric jitter around the capped exponential base", () => {
    const reconnectAttempt = 2;
    const baseDelay = Math.min(
      INITIAL_RECONNECT_DELAY * Math.pow(RECONNECT_MULTIPLIER, reconnectAttempt),
      MAX_RECONNECT_DELAY,
    );

    expect(getReconnectDelay(reconnectAttempt, () => 0)).toBe(
      baseDelay * (1 - RECONNECT_JITTER_FACTOR),
    );
    expect(getReconnectDelay(reconnectAttempt, () => 0.5)).toBe(baseDelay);
    expect(getReconnectDelay(reconnectAttempt, () => 1)).toBe(
      baseDelay * (1 + RECONNECT_JITTER_FACTOR),
    );
  });

  it("clamps jittered delays to the documented minimum and maximum", () => {
    expect(getReconnectDelay(-20, () => 0)).toBe(MIN_RECONNECT_DELAY);

    const cappedAttempt = 10;
    const cappedBase = Math.min(
      INITIAL_RECONNECT_DELAY * Math.pow(RECONNECT_MULTIPLIER, cappedAttempt),
      MAX_RECONNECT_DELAY,
    );

    expect(cappedBase).toBe(MAX_RECONNECT_DELAY);
    expect(getReconnectDelay(cappedAttempt, () => 1)).toBe(MAX_RECONNECT_DELAY);
  });
});

describe("useEventSource", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-01-01T00:00:00Z"));
    vi.spyOn(Math, "random").mockReturnValue(0.5);
    vi.clearAllMocks();
    MockEventSource.instances = [];
    connectionActions.length = 0;
    resetStores();
    vi.stubGlobal("EventSource", MockEventSource);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
    vi.useRealTimers();
    sseStore.setState({ handlers: new Map(), lastEventId: null, reconnectAttempt: 0 });
  });

  it("reconnects a silent stream and resets MCP before creating the replacement EventSource", async () => {
    const hook = await mountEventSourceHook();
    const first = latestEventSource();
    expect(first.url).toBe("http://djinn.test/events");
    expect(fetchActiveSnapshot).toHaveBeenCalledWith(["djinnos/djinn"]);
    expect(fetchClosedFirstPage).toHaveBeenCalledWith(["djinnos/djinn"]);

    act(() => first.onopen?.());
    await advanceTimersBy(SILENCE_TIMEOUT_MS);

    expect(first.close).toHaveBeenCalledTimes(1);
    expect(sseStore.getState().connectionStatus).toBe("reconnecting");
    expect(resetMcpClient).not.toHaveBeenCalled();
    expect(MockEventSource.instances).toHaveLength(1);

    await advanceTimersBy(INITIAL_RECONNECT_DELAY);

    expect(resetMcpClient).toHaveBeenCalledTimes(1);
    expect(MockEventSource.instances).toHaveLength(2);
    expect(connectionActions.slice(-2)).toEqual([
      "reset-mcp",
      "connect:http://djinn.test/events",
    ]);

    hook.unmount();
  });

  it("replays Last-Event-ID as an encoded query parameter on reconnect", async () => {
    const hook = await mountEventSourceHook();
    const first = latestEventSource();

    act(() => {
      sseStore.getState().setLastEventId("evt 42/?:&=");
      first.onerror?.();
    });

    await advanceTimersBy(INITIAL_RECONNECT_DELAY);

    expect(latestEventSource().url).toBe(
      "http://djinn.test/events?lastEventId=evt%2042%2F%3F%3A%26%3D",
    );

    hook.unmount();
  });

  it.each([
    ["ping", "{}"],
    ["lagged", "{}"],
    [
      "task.created",
      JSON.stringify({
        entity_type: "task",
        payload: { task: { id: "task-1", title: "Task 1" } },
      }),
    ],
    ["note.created", "{}"],
  ])("refreshes liveness on %s events before the silence threshold", async (eventType, payload) => {
    const hook = await mountEventSourceHook();
    const first = latestEventSource();
    act(() => first.onopen?.());

    await advanceTimersBy(SILENCE_TIMEOUT_MS - LIVENESS_WATCHDOG_INTERVAL_MS);
    expect(first.close).not.toHaveBeenCalled();

    act(() => {
      first.dispatch(eventType, payload);
    });

    await advanceTimersBy(SILENCE_TIMEOUT_MS - LIVENESS_WATCHDOG_INTERVAL_MS);
    expect(first.close).not.toHaveBeenCalled();
    expect(MockEventSource.instances).toHaveLength(1);

    await advanceTimersBy(LIVENESS_WATCHDOG_INTERVAL_MS);
    expect(first.close).toHaveBeenCalledTimes(1);

    hook.unmount();
  });

  it("stores Last-Event-ID from dispatchable domain events while refreshing liveness", async () => {
    const received: SSEEvent[] = [];
    sseStore.getState().subscribe("task_created", (event) => received.push(event));
    const hook = await mountEventSourceHook();
    const first = latestEventSource();

    act(() => {
      first.dispatch(
        "task.created",
        JSON.stringify({
          entity_type: "task",
          payload: { task: { id: "task-2", title: "Task 2" } },
        }),
        "event-123",
      );
    });

    expect(sseStore.getState().lastEventId).toBe("event-123");
    expect(received.at(-1)).toMatchObject({ type: "task_created", id: "event-123" });

    hook.unmount();
  });

  it("closes active streams and clears timers on cleanup without reconnecting", async () => {
    const hook = await mountEventSourceHook();
    const first = latestEventSource();

    hook.unmount();
    expect(first.close).toHaveBeenCalledTimes(1);

    await advanceTimersBy(SILENCE_TIMEOUT_MS + INITIAL_RECONNECT_DELAY);

    expect(resetMcpClient).not.toHaveBeenCalled();
    expect(MockEventSource.instances).toHaveLength(1);
  });

  it("clears a pending reconnect timer on cleanup without duplicate reconnect attempts", async () => {
    const hook = await mountEventSourceHook();
    const first = latestEventSource();

    act(() => {
      first.onerror?.();
    });
    expect(first.close).toHaveBeenCalledTimes(1);

    hook.unmount();
    expect(first.close).toHaveBeenCalledTimes(1);

    await advanceTimersBy(INITIAL_RECONNECT_DELAY + SILENCE_TIMEOUT_MS);

    expect(resetMcpClient).not.toHaveBeenCalled();
    expect(MockEventSource.instances).toHaveLength(1);
  });
});
