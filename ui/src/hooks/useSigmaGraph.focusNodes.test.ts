/**
 * useSigmaGraph focus helpers — camera-fit behavior for citation focus.
 *
 * Sigma is mocked because jsdom has no WebGL context. The graph is marked
 * as precomputed so these tests exercise only the imperative handle rather
 * than the FA2 supervisor path.
 */

import { act, cleanup, render } from "@testing-library/react";
import { createElement, createRef } from "react";
import Graph from "graphology";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { PRECOMPUTED_LAYOUT_ATTRIBUTE } from "@/lib/codeGraphAdapter";

const sigmaInstances: {
  camera: {
    ratio: number;
    animate: ReturnType<typeof vi.fn>;
    animatedReset: ReturnType<typeof vi.fn>;
  };
  killed: boolean;
  refresh: ReturnType<typeof vi.fn>;
  getSetting: ReturnType<typeof vi.fn>;
}[] = [];

vi.mock("sigma", () => {
  class MockSigma {
    camera = {
      ratio: 1,
      animate: vi.fn(),
      animatedReset: vi.fn(),
    };
    killed = false;
    refresh = vi.fn();
    getSetting = vi.fn((key: string) => {
      if (key === "minCameraRatio") return 0.002;
      if (key === "maxCameraRatio") return 50;
      return undefined;
    });
    getCamera() {
      return this.camera;
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
    constructor() {
      sigmaInstances.push(this);
    }
  }
  return { default: MockSigma };
});

vi.mock("@sigma/edge-curve", () => ({
  default: class MockEdgeCurveProgram {},
}));

vi.mock("sigma/rendering", () => ({
  EdgeRectangleProgram: class MockEdgeRectangleProgram {},
}));

vi.mock("graphology-layout-forceatlas2/worker", () => ({
  default: class MockSupervisor {
    start = vi.fn();
    stop = vi.fn();
    kill = vi.fn();
  },
}));

vi.mock("graphology-layout-forceatlas2", () => ({
  default: {
    inferSettings: () => ({}),
  },
}));

vi.mock("graphology-layout-noverlap", () => ({
  default: {
    assign: vi.fn(),
  },
}));

import type {
  SigmaInstanceHandle,
  UseSigmaGraphResult,
} from "@/hooks/useSigmaGraph";
import { useSigmaGraph } from "@/hooks/useSigmaGraph";

function makeGraph(): Graph {
  const graph = new Graph({ multi: true, type: "directed" });
  graph.setAttribute(PRECOMPUTED_LAYOUT_ATTRIBUTE, true);
  graph.addNode("a", { label: "a", x: 2, y: 4 });
  graph.addNode("b", { label: "b", x: 10, y: 16 });
  graph.addNode("same", { label: "same", x: 2, y: 4 });
  graph.addNode("bad", { label: "bad", x: Number.NaN, y: 4 });
  return graph;
}

function mountHarness(graph: Graph): {
  current: UseSigmaGraphResult | null;
  unmount: () => void;
} {
  const resultRef: { current: UseSigmaGraphResult | null } = { current: null };
  const containerRef = createRef<HTMLDivElement>();
  function CapturingHarness() {
    // eslint-disable-next-line react-hooks/immutability -- test harness intentionally captures the hook result into an outer holder for assertions.
    resultRef.current = useSigmaGraph(containerRef, graph);
    return createElement("div", { ref: containerRef });
  }

  let unmount = () => {};
  act(() => {
    ({ unmount } = render(createElement(CapturingHarness)));
  });

  return {
    get current() {
      return resultRef.current;
    },
    unmount,
  };
}

function mountedHandle(): {
  handle: SigmaInstanceHandle;
  animate: ReturnType<typeof vi.fn>;
  unmount: () => void;
} {
  const result = mountHarness(makeGraph());
  const handle = result.current?.sigma;
  if (!handle) throw new Error("expected Sigma handle to mount");
  const animate = sigmaInstances[0]!.camera.animate;
  // The selection-cache nudge effect also uses camera.animate on mount.
  // Clear it so each assertion observes only the explicit focus call.
  animate.mockClear();
  return { handle, animate, unmount: result.unmount };
}

describe("useSigmaGraph focus helpers", () => {
  beforeEach(() => {
    sigmaInstances.length = 0;
  });

  afterEach(() => {
    cleanup();
    vi.useRealTimers();
  });

  it("focusNode uses the default duration when no custom duration is provided", () => {
    const { handle, animate } = mountedHandle();

    handle.focusNode("a");

    expect(animate).toHaveBeenCalledTimes(1);
    expect(animate).toHaveBeenCalledWith(
      { x: 2, y: 4, ratio: 0.5 },
      { duration: 400 },
    );
  });

  it("focusNode uses the custom duration when durationMs is provided", () => {
    const { handle, animate } = mountedHandle();

    handle.focusNode("a", 150);

    expect(animate).toHaveBeenCalledTimes(1);
    expect(animate).toHaveBeenCalledWith(
      { x: 2, y: 4, ratio: 0.5 },
      { duration: 150 },
    );
  });

  it("focusNodes fits the union bbox of valid ids and silently drops missing ids", () => {
    const { handle, animate } = mountedHandle();

    handle.focusNodes(["missing", "a", "b"]);

    expect(animate).toHaveBeenCalledTimes(1);
    expect(animate).toHaveBeenCalledWith(
      { x: 6, y: 10, ratio: 18 },
      { duration: 400 },
    );
  });

  it("clamps the focusNodes ratio to the configured camera bounds", () => {
    const { handle, animate } = mountedHandle();

    handle.focusNodes(["a", "same"]);

    expect(animate).toHaveBeenCalledTimes(1);
    expect(animate).toHaveBeenCalledWith(
      { x: 2, y: 4, ratio: 0.002 },
      { duration: 400 },
    );
  });

  it("no-ops when focusNodes receives an empty iterable or no resolvable positions", () => {
    const { handle, animate } = mountedHandle();

    handle.focusNodes([]);
    handle.focusNodes(["missing", "bad"]);
    handle.focusNode("missing");
    handle.focusNode("bad");

    expect(animate).not.toHaveBeenCalled();
  });

  it("no-ops after the Sigma handle has been killed", () => {
    const { handle, animate, unmount } = mountedHandle();

    act(() => {
      unmount();
    });
    handle.focusNode("a");
    handle.focusNodes(["a", "b"]);

    expect(animate).not.toHaveBeenCalled();
  });
});
