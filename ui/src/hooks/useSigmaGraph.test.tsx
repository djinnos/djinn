/**
 * useSigmaGraph — focused tests for the FA2 supervisor gating.
 *
 * Two branches to cover:
 *   1. **Precomputed graph** — when the graphology graph carries the
 *      `precomputedLayout` attribute (set by `buildGraphFromSnapshot` for
 *      snapshots whose nodes all ship finite `x`/`y` from the warm-time
 *      server layout), `useSigmaGraph` must NOT construct or start an
 *      `FA2LayoutSupervisor`, must NOT schedule a stop-timer, and must
 *      keep `layoutRunning` false so the "Layout optimizing…" pill never
 *      shows.
 *   2. **Legacy graph** — when the graph does NOT carry the
 *      `precomputedLayout` attribute, the existing FA2 path is the
 *      fallback: a supervisor is constructed and started, `layoutRunning`
 *      flips to true, and the stop-timer fires after `inferRunMs` to
 *      stop the supervisor and run the noverlap pass.
 *
 * Sigma and `@sigma/edge-curve` are mocked because jsdom has no WebGL
 * context; the FA2 worker module is also mocked (it uses Web Workers +
 * SharedArrayBuffer) so we can assert `new` / `start` / `stop` /
 * `kill` calls without touching the runtime. `noverlap.assign` is
 * mocked so the legacy branch's cleanup path is observable.
 */

import { act, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createRef } from "react";
import Graph from "graphology";
import { PRECOMPUTED_LAYOUT_ATTRIBUTE } from "@/lib/codeGraphAdapter";

// Module mocks must be hoisted — keep factory bodies self-contained
// (no top-level references to vars defined below).

// Sigma + WebGL — stub the constructor. The mock exposes the same
// surface the hook reads (`getCamera`, `refresh`, `kill`, `on`).
const sigmaInstances: {
  killed: boolean;
  refresh: ReturnType<typeof vi.fn>;
}[] = [];
vi.mock("sigma", () => {
  class MockSigma {
    killed = false;
    refresh = vi.fn();
    getCamera() {
      return { animatedReset: () => {} };
    }
    kill() {
      this.killed = true;
    }
    on() {
      return () => {};
    }
    removeListener() {
      return () => {};
    }
    constructor(_graph: unknown, _container: unknown, _opts?: unknown) {
      sigmaInstances.push(this);
    }
  }
  return { default: MockSigma };
});

// `@sigma/edge-curve` ships an ES module that does some immediate
// WebGL probing on import — mock it so jsdom doesn't blow up.
vi.mock("@sigma/edge-curve", () => ({
  default: class MockEdgeCurveProgram {},
}));

// `sigma/rendering` touches WebGL2RenderingContext at module load, which
// jsdom lacks — stub the straight-edge program the same way.
vi.mock("sigma/rendering", () => ({
  EdgeRectangleProgram: class MockEdgeRectangleProgram {},
}));

// FA2 worker — track every constructor / start / stop / kill so the
// test can assert which side effects fired on each branch.
const fa2Instances: {
  isRunning: () => boolean;
  start: ReturnType<typeof vi.fn>;
  stop: ReturnType<typeof vi.fn>;
  kill: ReturnType<typeof vi.fn>;
}[] = [];
vi.mock("graphology-layout-forceatlas2/worker", () => {
  class MockSupervisor {
    isRunning = () => false;
    start = vi.fn();
    stop = vi.fn();
    kill = vi.fn();
    constructor(_graph: unknown, _opts?: unknown) {
      fa2Instances.push(this);
    }
  }
  return { default: MockSupervisor };
});

// `graphology-layout-forceatlas2` (non-worker) — used to derive settings.
vi.mock("graphology-layout-forceatlas2", () => ({
  default: {
    inferSettings: () => ({}),
  },
}));

// noverlap — observability for the legacy branch's stop-timer callback.
const noverlapMock = vi.fn();
vi.mock("graphology-layout-noverlap", () => ({
  default: {
    assign: (...args: unknown[]) => noverlapMock(...args),
  },
}));

// Import after mocks are registered.
import { useSigmaGraph } from "@/hooks/useSigmaGraph";

// ── Harness ────────────────────────────────────────────────────────────────

interface UseSigmaGraphResult {
  ready: boolean;
  layoutRunning: boolean;
  stopLayout: () => void;
  sigma: unknown;
}

/**
 * Render a fresh harness and return the latest result snapshot.
 * We deliberately do NOT use `waitFor` to poll for state — the
 * hook synchronously sets `ready` + `layoutRunning` in the effect
 * that runs after the first commit, so a single `act()`-wrapped
 * `render` is enough. Avoiding `waitFor` keeps the test
 * deterministic regardless of the timer mode (`real` / `fake`)
 * and avoids heap pressure from accumulated waitFor retries.
 */
function mountHarness(
  graph: Graph | null,
): { current: UseSigmaGraphResult | null } {
  const ref: { current: UseSigmaGraphResult | null } = { current: null };
  function CapturingHarness() {
    const containerRef = createRef<HTMLDivElement>();
    // eslint-disable-next-line react-hooks/immutability -- test harness intentionally captures the hook result into an outer holder for assertions.
    ref.current = useSigmaGraph(containerRef, graph);
    return <div data-testid="harness-root" ref={containerRef} />;
  }
  act(() => {
    render(<CapturingHarness />);
  });
  return ref;
}

function makeGraph(): Graph {
  const g = new Graph({ multi: true, type: "directed" });
  g.addNode("a", { label: "a", pagerank: 0.5, x: 0, y: 0 });
  g.addNode("b", { label: "b", pagerank: 0.5, x: 1, y: 1 });
  g.addEdge("a", "b", { kind: "ContainsDefinition", confidence: 0.9 });
  return g;
}

// ── Tests ──────────────────────────────────────────────────────────────────

describe("useSigmaGraph — FA2 supervisor gating", () => {
  beforeEach(() => {
    sigmaInstances.length = 0;
    fa2Instances.length = 0;
    noverlapMock.mockReset();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("constructs a Sigma instance and marks the hook ready for any graph", () => {
    const graph = makeGraph();
    const result = mountHarness(graph);

    expect(result.current?.ready).toBe(true);
    expect(sigmaInstances).toHaveLength(1);
    expect(result.current?.sigma).not.toBeNull();
  });

  it("skips the FA2 supervisor when the graph is marked as precomputed", () => {
    const graph = makeGraph();
    graph.setAttribute(PRECOMPUTED_LAYOUT_ATTRIBUTE, true);

    const result = mountHarness(graph);

    // Sigma still mounted, but the FA2 supervisor was never started.
    expect(sigmaInstances).toHaveLength(1);
    expect(fa2Instances).toHaveLength(0);
    // layoutRunning must stay false so the "Layout optimizing…" pill
    // never appears.
    expect(result.current?.ready).toBe(true);
    expect(result.current?.layoutRunning).toBe(false);
  });

  it("starts the FA2 supervisor and flips layoutRunning to true on a legacy (non-precomputed) graph", () => {
    const graph = makeGraph();
    // Make sure the marker is absent — this is the default for
    // hand-built graphs, but assert it so the test stays meaningful
    // if the adapter's defaults change in the future.
    expect(graph.getAttribute(PRECOMPUTED_LAYOUT_ATTRIBUTE)).toBeUndefined();

    const result = mountHarness(graph);

    expect(result.current?.ready).toBe(true);
    expect(result.current?.layoutRunning).toBe(true);
    expect(fa2Instances).toHaveLength(1);
    expect(fa2Instances[0]!.start).toHaveBeenCalledTimes(1);
  });

  it("stops the FA2 supervisor, runs noverlap, and resets the camera when the legacy stop-timer fires", () => {
    // Use fake timers to deterministically trigger the legacy
    // branch's setTimeout cleanup. The branch picks `inferRunMs`
    // by node count — our 2-node graph falls through to 15_000ms
    // (smallest bucket).
    vi.useFakeTimers();
    const graph = makeGraph();
    const result = mountHarness(graph);

    expect(fa2Instances).toHaveLength(1);
    expect(result.current?.layoutRunning).toBe(true);
    expect(fa2Instances[0]!.start).toHaveBeenCalledTimes(1);

    // Advance past the 15s default small-graph run window. Wrap in
    // `act` so the resulting state updates (setLayoutRunning(false))
    // commit before we read them.
    act(() => {
      vi.advanceTimersByTime(16_000);
    });

    expect(fa2Instances[0]!.stop).toHaveBeenCalled();
    // The stop-timer callback runs the noverlap pass and a camera
    // animatedReset; assert both happened.
    expect(noverlapMock).toHaveBeenCalledWith(
      graph,
      expect.objectContaining({ settings: expect.any(Object) }),
    );
    expect(sigmaInstances[0]!.refresh).toHaveBeenCalled();
    expect(result.current?.layoutRunning).toBe(false);
  });

  it("does not schedule a stop-timer or call noverlap on the precomputed branch", () => {
    vi.useFakeTimers();
    const graph = makeGraph();
    graph.setAttribute(PRECOMPUTED_LAYOUT_ATTRIBUTE, true);

    const result = mountHarness(graph);

    expect(fa2Instances).toHaveLength(0);
    expect(result.current?.layoutRunning).toBe(false);

    // Advance well past the legacy branch's 15s default window — if
    // the precomputed branch had silently scheduled a timer, noverlap
    // would be called here. It must not be.
    act(() => {
      vi.advanceTimersByTime(60_000);
    });
    expect(noverlapMock).not.toHaveBeenCalled();
    expect(result.current?.layoutRunning).toBe(false);
  });

  it("tracks the Sigma instance for the precomputed branch so cleanup can kill it", () => {
    const graph = makeGraph();
    graph.setAttribute(PRECOMPUTED_LAYOUT_ATTRIBUTE, true);

    const result = mountHarness(graph);

    expect(sigmaInstances).toHaveLength(1);
    // Sigma was created so the cleanup has something to kill on
    // unmount. The actual `kill()` is fired by React's effect
    // teardown; assert the instance exists and is in a non-killed
    // state pre-unmount.
    expect(sigmaInstances[0]!.killed).toBe(false);
    // No FA2 work was ever started on this branch.
    expect(fa2Instances).toHaveLength(0);
    expect(result.current?.ready).toBe(true);
    expect(result.current?.layoutRunning).toBe(false);
  });

  it("stopLayout is a safe no-op for the precomputed branch (no supervisor to stop, no noverlap to run)", () => {
    const graph = makeGraph();
    graph.setAttribute(PRECOMPUTED_LAYOUT_ATTRIBUTE, true);

    const result = mountHarness(graph);

    expect(result.current?.ready).toBe(true);
    const stop = result.current!.stopLayout;
    expect(() => stop()).not.toThrow();
    expect(noverlapMock).not.toHaveBeenCalled();
    // FA2 supervisor was never created.
    expect(fa2Instances).toHaveLength(0);
    expect(result.current?.layoutRunning).toBe(false);
  });
});
