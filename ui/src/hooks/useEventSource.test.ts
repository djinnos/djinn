import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  calculateReconnectDelay,
  INITIAL_RECONNECT_DELAY,
  LIVENESS_CHECK_INTERVAL_MS,
  MAX_RECONNECT_DELAY,
  RECONNECT_JITTER_RATIO,
  SILENCE_TIMEOUT_MS,
  useEventSource,
} from "./useEventSource";
import { resetMcpClient } from "@/api/mcpClient";
import { sseStore } from "@/stores/sseStore";
import { projectStore } from "@/stores/projectStore";

vi.mock("@/api/server", () => ({
  fetchKanbanSnapshot: vi.fn().mockResolvedValue({ tasks: [], epics: [] }),
}));

vi.mock("@/api/mcpClient", () => ({
  resetMcpClient: vi.fn().mockResolvedValue(undefined),
}));

vi.mock("@/stores/dispatchPauseStore", async (importOriginal) => {
  const actual =
    await importOriginal<typeof import("@/stores/dispatchPauseStore")>();
  return {
    ...actual,
    refreshDispatchPauseStatus: vi.fn().mockResolvedValue(undefined),
  };
});

type EventSourceListener = (event: MessageEvent) => void;

class MockEventSource {
  static instances: MockEventSource[] = [];

  readonly listeners = new Map<string, Set<EventSourceListener>>();
  onopen: ((event: Event) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  closed = false;

  constructor(readonly url: string) {
    MockEventSource.instances.push(this);
  }

  addEventListener(type: string, listener: EventSourceListener): void {
    const listeners =
      this.listeners.get(type) ?? new Set<EventSourceListener>();
    listeners.add(listener);
    this.listeners.set(type, listeners);
  }

  close(): void {
    this.closed = true;
  }

  open(): void {
    this.onopen?.(new Event("open"));
  }

  error(): void {
    this.onerror?.(new Event("error"));
  }

  emit(type: string, data = "{}", lastEventId = ""): void {
    const event = { data, lastEventId } as MessageEvent;
    this.listeners.get(type)?.forEach((listener) => listener(event));
  }
}

async function flushPromises(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}

describe("useEventSource", () => {
  const originalEventSource = globalThis.EventSource;
  const originalRandom = Math.random;

  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(0);
    vi.spyOn(Math, "random").mockReturnValue(0.5);
    vi.clearAllMocks();
    MockEventSource.instances = [];
    globalThis.EventSource = MockEventSource as unknown as typeof EventSource;
    projectStore.setState({ projects: [] });
    sseStore.setState({
      isConnected: false,
      connectionStatus: "reconnecting",
      lastError: null,
      reconnectAttempt: 0,
      lastEventId: null,
      handlers: new Map(),
    });
  });

  afterEach(() => {
    globalThis.EventSource = originalEventSource;
    Math.random = originalRandom;
    vi.clearAllTimers();
    vi.useRealTimers();
  });

  it("bounds reconnect jitter around the capped exponential backoff", () => {
    const firstBase = INITIAL_RECONNECT_DELAY;
    expect(calculateReconnectDelay(0, () => 0)).toBe(
      firstBase * (1 - RECONNECT_JITTER_RATIO),
    );
    expect(calculateReconnectDelay(0, () => 1)).toBe(
      firstBase * (1 + RECONNECT_JITTER_RATIO),
    );

    const cappedLow = MAX_RECONNECT_DELAY * (1 - RECONNECT_JITTER_RATIO);
    expect(calculateReconnectDelay(10, () => 0)).toBe(cappedLow);
    expect(calculateReconnectDelay(10, () => 1)).toBe(MAX_RECONNECT_DELAY);
  });

  it("force-closes and reconnects silent streams with Last-Event-ID replay", async () => {
    renderHook(() => useEventSource());
    await act(async () => {
      await flushPromises();
    });

    const first = MockEventSource.instances[0];
    expect(first.url).toBe("http://localhost:3000/events");

    act(() => {
      first.open();
      sseStore.getState().setLastEventId("evt 123");
    });

    await act(async () => {
      await vi.advanceTimersByTimeAsync(
        SILENCE_TIMEOUT_MS - LIVENESS_CHECK_INTERVAL_MS,
      );
    });
    expect(first.closed).toBe(false);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(LIVENESS_CHECK_INTERVAL_MS);
    });
    expect(first.closed).toBe(true);
    expect(sseStore.getState().isConnected).toBe(false);
    expect(sseStore.getState().connectionStatus).toBe("reconnecting");
    expect(MockEventSource.instances).toHaveLength(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(INITIAL_RECONNECT_DELAY);
      await flushPromises();
    });

    expect(resetMcpClient).toHaveBeenCalledTimes(1);
    expect(MockEventSource.instances).toHaveLength(2);
    expect(MockEventSource.instances[1].url).toBe(
      "http://localhost:3000/events?lastEventId=evt%20123",
    );
  });

  it("treats ping, lagged, and mapped domain events as liveness", async () => {
    renderHook(() => useEventSource());
    await act(async () => {
      await flushPromises();
    });

    const first = MockEventSource.instances[0];
    act(() => {
      first.open();
    });

    await act(async () => {
      await vi.advanceTimersByTimeAsync(
        SILENCE_TIMEOUT_MS - LIVENESS_CHECK_INTERVAL_MS,
      );
    });
    expect(first.closed).toBe(false);

    act(() => {
      first.emit("ping");
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(
        SILENCE_TIMEOUT_MS - LIVENESS_CHECK_INTERVAL_MS,
      );
    });
    expect(first.closed).toBe(false);

    act(() => {
      first.emit("lagged");
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(
        SILENCE_TIMEOUT_MS - LIVENESS_CHECK_INTERVAL_MS,
      );
    });
    expect(first.closed).toBe(false);

    act(() => {
      first.emit(
        "task.updated",
        JSON.stringify({
          entity_type: "task",
          action: "updated",
          payload: { task: { id: "t1" } },
        }),
        "evt-2",
      );
    });
    expect(sseStore.getState().lastEventId).toBe("evt-2");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(
        SILENCE_TIMEOUT_MS - LIVENESS_CHECK_INTERVAL_MS,
      );
    });
    expect(first.closed).toBe(false);
  });

  it("cleans up EventSource and timers without leaving reconnect attempts", async () => {
    const { unmount } = renderHook(() => useEventSource());
    await act(async () => {
      await flushPromises();
    });

    const first = MockEventSource.instances[0];
    act(() => {
      first.open();
      unmount();
    });

    expect(first.closed).toBe(true);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(
        SILENCE_TIMEOUT_MS + INITIAL_RECONNECT_DELAY,
      );
      await flushPromises();
    });

    expect(resetMcpClient).not.toHaveBeenCalled();
    expect(MockEventSource.instances).toHaveLength(1);
  });

  it("uses the same jittered reconnect flow for EventSource errors", async () => {
    renderHook(() => useEventSource());
    await act(async () => {
      await flushPromises();
    });

    const first = MockEventSource.instances[0];
    act(() => {
      first.open();
      first.error();
    });

    expect(first.closed).toBe(true);
    expect(sseStore.getState().connectionStatus).toBe("reconnecting");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(INITIAL_RECONNECT_DELAY);
      await flushPromises();
    });

    expect(resetMcpClient).toHaveBeenCalledTimes(1);
    expect(MockEventSource.instances).toHaveLength(2);
  });
});
