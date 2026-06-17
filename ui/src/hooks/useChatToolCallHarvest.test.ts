/**
 * useChatToolCallHarvest — D5 producer tests.
 *
 * Two surface areas:
 *   1. **Pure helpers** (`fuzzyResolveIds`, `extractIdsFromToolResult`,
 *      `harvestMessageToolCalls`, `isCodeGraphToolCallName`) are tested
 *      directly so the wire-shape fuzzing stays deterministic.
 *   2. **Hook integration** — `fetchSnapshot` is mocked, `useChatStore`
 *      is seeded with synthetic messages, and we assert the eventual
 *      `setCitations` call carries the expected deduped resolved set.
 *
 * Snapshot fetch is mocked at the `@/api/codeGraph` boundary because
 * that's where the production hook reaches for it. The hook's behavior
 * degrades on snapshot fetch errors and on empty snapshot payloads —
 * both branches get explicit coverage.
 */

import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useChatStore, type ChatMessage } from "@/stores/chatStore";
import { useCodeGraphStore } from "@/stores/codeGraphStore";

import {
  extractIdsFromToolResult,
  fuzzyResolveIds,
  harvestMessageToolCalls,
  isCodeGraphToolCallName,
  useChatToolCallHarvest,
} from "./useChatToolCallHarvest";

const { fetchSnapshotMock } = vi.hoisted(() => ({
  fetchSnapshotMock: vi.fn(),
}));

vi.mock("@/api/codeGraph", async () => {
  const actual = await vi.importActual<typeof import("@/api/codeGraph")>(
    "@/api/codeGraph",
  );
  return {
    ...actual,
    fetchSnapshot: (...args: unknown[]) => fetchSnapshotMock(...args),
  };
});

// ── Fixtures ─────────────────────────────────────────────────────────────────

const SESSION = "session-1";
const PROJECT = "djinnos/djinn";

const SNAPSHOT = {
  snapshot: {
    project_id: "p-1",
    git_head: "abc",
    generated_at: "2026-01-01",
    truncated: false,
    total_nodes: 5,
    total_edges: 0,
    node_cap: 5,
    nodes: [
      { id: "scip-typescript npm pkg 1.0.0 src/Foo#", kind: "symbol" },
      { id: "scip-typescript npm pkg 1.0.0 src/Bar#", kind: "symbol" },
      { id: "file:src/baz.rs", kind: "file" },
      { id: "scip-rust crates lib 0.1.0 src/. qux().", kind: "symbol" },
      { id: "scip-rust crates lib 0.1.0 src/. quux().", kind: "symbol" },
    ],
    edges: [],
  },
};

function makeImpactMessage(
  id: string,
  result: { output: string; success: boolean },
): ChatMessage {
  return {
    id,
    role: "assistant",
    content: "impact result",
    toolCalls: [{ name: "impact", input: { key: "Foo#" }, success: result.success, result }],
    createdAt: 1,
  };
}

function makeNeighborsMessage(
  id: string,
  result: { output: string; success: boolean },
): ChatMessage {
  return {
    id,
    role: "assistant",
    content: "neighbors result",
    toolCalls: [{ name: "neighbors", result }],
    createdAt: 2,
  };
}

function makeSearchMessage(
  id: string,
  result: { output: string; success: boolean },
): ChatMessage {
  return {
    id,
    role: "assistant",
    content: "search result",
    toolCalls: [{ name: "search", result }],
    createdAt: 3,
  };
}

// ── Helpers ─────────────────────────────────────────────────────────────────

async function flushMicrotasks(): Promise<void> {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

function resetStores(): void {
  useChatStore.setState({
    sessions: [],
    messagesBySession: {},
    streamingBySession: {},
    loadingBySession: {},
    thinkingStartTimeBySession: {},
    draftBySession: {},
    globalDraft: "",
    activeSessionId: SESSION,
  });
  useCodeGraphStore.getState().reset();
  // The reset() helper preserves `selectedWorkspaceSlug` but resets
  // everything else; explicitly clear the citation set too so each
  // test starts from a known empty baseline.
  useCodeGraphStore.setState({ citationIds: new Set() });
  fetchSnapshotMock.mockReset();
  fetchSnapshotMock.mockResolvedValue(SNAPSHOT);
}

function mountHarvest(slug: string | null = PROJECT) {
  return renderHook(
    ({ slug }: { slug: string | null }) =>
      useChatToolCallHarvest({ projectSlug: slug }),
    { initialProps: { slug } },
  );
}

// ── Pure-helper tests ───────────────────────────────────────────────────────

describe("isCodeGraphToolCallName", () => {
  it.each([
    ["impact", true],
    ["neighbors", true],
    ["search", true],
    ["code_graph", true],
    ["code_graph_search", true],
    ["code_graph_impact", true],
    ["other_tool", false],
    ["", false],
    [undefined, false],
  ])("returns %s for %p", (input, expected) => {
    expect(isCodeGraphToolCallName(input as string | undefined)).toBe(expected);
  });
});

describe("extractIdsFromToolResult", () => {
  it("extracts the seed key plus impact entries (key form)", () => {
    const out = extractIdsFromToolResult(
      "impact",
      JSON.stringify({
        key: "Foo#",
        impact: [
          { key: "Foo#", depth: 0 },
          { key: "Bar#", depth: 1 },
        ],
      }),
    );
    expect(out).toEqual(new Set(["Foo#", "Bar#"]));
  });

  it("accepts the uid alias for impact entries", () => {
    const out = extractIdsFromToolResult(
      "impact",
      JSON.stringify({
        uid: "Foo#",
        impact: [{ uid: "Bar#", depth: 1 }],
      }),
    );
    expect(out).toEqual(new Set(["Foo#", "Bar#"]));
  });

  it("extracts sample_keys from the file_groups form", () => {
    const out = extractIdsFromToolResult(
      "impact",
      JSON.stringify({
        key: "Foo#",
        file_groups: [
          { file: "src/foo.rs", sample_keys: ["a", "b"] },
          { file: "src/bar.rs", sample_keys: ["c"] },
        ],
      }),
    );
    expect(out).toEqual(new Set(["Foo#", "a", "b", "c"]));
  });

  it("extracts neighbor keys from a neighbors payload", () => {
    const out = extractIdsFromToolResult(
      "neighbors",
      JSON.stringify({
        neighbors: [
          { key: "Foo#", kind: "function" },
          { key: "Bar#", kind: "function", uid: "Bar#" },
        ],
      }),
    );
    expect(out).toEqual(new Set(["Foo#", "Bar#"]));
  });

  it("extracts hit keys from a search payload", () => {
    const out = extractIdsFromToolResult(
      "search",
      JSON.stringify({
        hits: [
          { key: "Foo#", score: 0.9 },
          { key: "Bar#", score: 0.7 },
        ],
      }),
    );
    expect(out).toEqual(new Set(["Foo#", "Bar#"]));
  });

  it("returns null for unparseable JSON", () => {
    expect(extractIdsFromToolResult("impact", "{not json")).toBeNull();
  });

  it("returns null for a payload that lacks the expected fields", () => {
    expect(
      extractIdsFromToolResult("impact", JSON.stringify({ risk: "HIGH" })),
    ).toBeNull();
  });

  it("skips payloads whose success envelope is false", () => {
    expect(
      extractIdsFromToolResult(
        "impact",
        JSON.stringify({ success: false, key: "Foo#", impact: [{ key: "Foo#" }] }),
      ),
    ).toBeNull();
  });

  it("returns null for non-code_graph tool names", () => {
    expect(
      extractIdsFromToolResult("read_file", JSON.stringify({ content: "x" })),
    ).toBeNull();
  });

  it("does not throw when extra fields are present", () => {
    const out = extractIdsFromToolResult(
      "impact",
      JSON.stringify({
        key: "Foo#",
        risk: "MEDIUM",
        summary: "lots of stuff",
        impact: [{ key: "Bar#", depth: 1, extra_field: 42 }],
        metadata: { source: "agent" },
      }),
    );
    expect(out).toEqual(new Set(["Foo#", "Bar#"]));
  });
});

describe("fuzzyResolveIds", () => {
  const valid = new Set<string>([
    "scip-typescript npm pkg 1.0.0 src/Foo#",
    "scip-typescript npm pkg 1.0.0 src/Bar#",
    "file:src/baz.rs",
  ]);

  it("returns exact matches verbatim", () => {
    const resolved = fuzzyResolveIds(["file:src/baz.rs"], valid);
    expect(resolved).toEqual(new Set(["file:src/baz.rs"]));
  });

  it("suffix-matches partial ids against the snapshot", () => {
    const resolved = fuzzyResolveIds(["Foo#", "Bar#"], valid);
    expect(resolved).toEqual(
      new Set([
        "scip-typescript npm pkg 1.0.0 src/Foo#",
        "scip-typescript npm pkg 1.0.0 src/Bar#",
      ]),
    );
  });

  it("drops ids that don't resolve", () => {
    const resolved = fuzzyResolveIds(["nope#", "also-nope"], valid);
    expect(resolved.size).toBe(0);
  });

  it("mixes exact and fuzzy resolutions in one call", () => {
    const resolved = fuzzyResolveIds(
      ["file:src/baz.rs", "Foo#", "missing#"],
      valid,
    );
    expect(resolved).toEqual(
      new Set([
        "file:src/baz.rs",
        "scip-typescript npm pkg 1.0.0 src/Foo#",
      ]),
    );
  });

  it("returns an empty set when the snapshot allowlist is empty", () => {
    const resolved = fuzzyResolveIds(["anything"], new Set());
    expect(resolved.size).toBe(0);
  });

  it("skips empty / non-string candidates", () => {
    const resolved = fuzzyResolveIds(
      ["", "Foo#"] as unknown as string[],
      valid,
    );
    expect(resolved).toEqual(
      new Set(["scip-typescript npm pkg 1.0.0 src/Foo#"]),
    );
  });
});

describe("harvestMessageToolCalls", () => {
  it("collects ids across multiple tool calls in one message", () => {
    const message: ChatMessage = {
      id: "m1",
      role: "assistant",
      content: "",
      createdAt: 1,
      toolCalls: [
        {
          name: "impact",
          result: {
            output: JSON.stringify({ key: "Foo#", impact: [{ key: "Bar#" }] }),
            success: true,
          },
        },
        {
          name: "search",
          result: {
            output: JSON.stringify({ hits: [{ key: "Foo#" }] }),
            success: true,
          },
        },
      ],
    };
    expect(harvestMessageToolCalls(message)).toEqual(new Set(["Foo#", "Bar#"]));
  });

  it("skips calls with success:false even when structured data is present", () => {
    const message: ChatMessage = {
      id: "m1",
      role: "assistant",
      content: "",
      createdAt: 1,
      toolCalls: [
        {
          name: "impact",
          success: false,
          result: {
            output: JSON.stringify({ key: "Foo#", impact: [{ key: "Foo#" }] }),
            success: false,
          },
        },
      ],
    };
    expect(harvestMessageToolCalls(message).size).toBe(0);
  });

  it("returns an empty set when the message has no tool calls", () => {
    const message: ChatMessage = {
      id: "m1",
      role: "assistant",
      content: "no tools",
      createdAt: 1,
    };
    expect(harvestMessageToolCalls(message).size).toBe(0);
  });

  it("returns an empty set when every result is unparseable JSON", () => {
    const message: ChatMessage = {
      id: "m1",
      role: "assistant",
      content: "",
      createdAt: 1,
      toolCalls: [
        { name: "impact", result: { output: "not-json", success: true } },
      ],
    };
    expect(harvestMessageToolCalls(message).size).toBe(0);
  });

  it("processes the rest when one call's result is unparseable", () => {
    const message: ChatMessage = {
      id: "m1",
      role: "assistant",
      content: "",
      createdAt: 1,
      toolCalls: [
        { name: "impact", result: { output: "not-json", success: true } },
        {
          name: "search",
          result: {
            output: JSON.stringify({ hits: [{ key: "Foo#" }] }),
            success: true,
          },
        },
      ],
    };
    expect(harvestMessageToolCalls(message)).toEqual(new Set(["Foo#"]));
  });
});

// ── Hook integration tests ──────────────────────────────────────────────────

describe("useChatToolCallHarvest", () => {
  beforeEach(() => {
    resetStores();
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("no-ops when projectSlug is null", async () => {
    useChatStore.getState().setSessionMessages(SESSION, [
      makeImpactMessage("a1", {
        output: JSON.stringify({ key: "Foo#", impact: [{ key: "Bar#" }] }),
        success: true,
      }),
    ]);

    const { unmount } = mountHarvest(null);
    await flushMicrotasks();

    expect(fetchSnapshotMock).not.toHaveBeenCalled();
    expect(useCodeGraphStore.getState().citationIds.size).toBe(0);

    unmount();
  });

  it("populates citations for a finished assistant impact message", async () => {
    useChatStore.getState().setSessionMessages(SESSION, [
      makeImpactMessage("a1", {
        output: JSON.stringify({
          key: "Foo#",
          impact: [{ key: "Bar#" }, { key: "Foo#" }],
        }),
        success: true,
      }),
    ]);

    const { unmount } = mountHarvest();
    await flushMicrotasks();

    const ids = useCodeGraphStore.getState().citationIds;
    expect(ids).toEqual(
      new Set([
        "scip-typescript npm pkg 1.0.0 src/Foo#",
        "scip-typescript npm pkg 1.0.0 src/Bar#",
      ]),
    );

    unmount();
  });

  it("dedupes ids across multiple tool calls (impact + neighbors + search)", async () => {
    useChatStore.getState().setSessionMessages(SESSION, [
      makeImpactMessage("a1", {
        output: JSON.stringify({ key: "Foo#", impact: [{ key: "Bar#" }] }),
        success: true,
      }),
      makeNeighborsMessage("a2", {
        output: JSON.stringify({
          neighbors: [
            { key: "Foo#" },
            { key: "scip-rust crates lib 0.1.0 src/. qux()." },
          ],
        }),
        success: true,
      }),
      makeSearchMessage("a3", {
        output: JSON.stringify({
          hits: [{ key: "Bar#" }, { key: "nope#" }],
        }),
        success: true,
      }),
    ]);

    const { unmount } = mountHarvest();
    await flushMicrotasks();

    const ids = useCodeGraphStore.getState().citationIds;
    expect(ids).toEqual(
      new Set([
        "scip-typescript npm pkg 1.0.0 src/Foo#",
        "scip-typescript npm pkg 1.0.0 src/Bar#",
        "scip-rust crates lib 0.1.0 src/. qux().",
      ]),
    );

    unmount();
  });

  it("resolves partial ids via suffix match when the model emits short keys", async () => {
    useChatStore.getState().setSessionMessages(SESSION, [
      makeSearchMessage("a1", {
        output: JSON.stringify({
          hits: [{ key: "Bar#" }, { key: "Foo#" }],
        }),
        success: true,
      }),
    ]);

    const { unmount } = mountHarvest();
    await flushMicrotasks();

    const ids = useCodeGraphStore.getState().citationIds;
    expect(ids).toEqual(
      new Set([
        "scip-typescript npm pkg 1.0.0 src/Bar#",
        "scip-typescript npm pkg 1.0.0 src/Foo#",
      ]),
    );

    unmount();
  });

  it("skips calls whose result.success is false but processes the rest", async () => {
    useChatStore.getState().setSessionMessages(SESSION, [
      {
        id: "a1",
        role: "assistant",
        content: "",
        createdAt: 1,
        toolCalls: [
          {
            name: "impact",
            success: false,
            result: {
              output: JSON.stringify({
                key: "Foo#",
                impact: [{ key: "Foo#" }],
              }),
              success: false,
            },
          },
          {
            name: "search",
            result: {
              output: JSON.stringify({ hits: [{ key: "Bar#" }] }),
              success: true,
            },
          },
        ],
      },
    ]);

    const { unmount } = mountHarvest();
    await flushMicrotasks();

    const ids = useCodeGraphStore.getState().citationIds;
    expect(ids).toEqual(new Set(["scip-typescript npm pkg 1.0.0 src/Bar#"]));

    unmount();
  });

  it("no-ops when no ids resolve against the snapshot", async () => {
    useChatStore.getState().setSessionMessages(SESSION, [
      makeSearchMessage("a1", {
        output: JSON.stringify({
          hits: [{ key: "completely_unknown_key#" }],
        }),
        success: true,
      }),
    ]);

    const { unmount } = mountHarvest();
    await flushMicrotasks();

    expect(useCodeGraphStore.getState().citationIds.size).toBe(0);

    unmount();
  });

  it("no-ops when the snapshot fetch returns an empty payload", async () => {
    fetchSnapshotMock.mockResolvedValueOnce({
      snapshot: { nodes: [], edges: [] },
    });

    useChatStore.getState().setSessionMessages(SESSION, [
      makeImpactMessage("a1", {
        output: JSON.stringify({ key: "Foo#", impact: [{ key: "Bar#" }] }),
        success: true,
      }),
    ]);

    const { unmount } = mountHarvest();
    await flushMicrotasks();

    expect(useCodeGraphStore.getState().citationIds.size).toBe(0);

    unmount();
  });

  it("no-ops when the snapshot fetch rejects", async () => {
    fetchSnapshotMock.mockRejectedValueOnce(new Error("boom"));

    useChatStore.getState().setSessionMessages(SESSION, [
      makeImpactMessage("a1", {
        output: JSON.stringify({ key: "Foo#", impact: [{ key: "Bar#" }] }),
        success: true,
      }),
    ]);

    const { unmount } = mountHarvest();
    await flushMicrotasks();

    expect(useCodeGraphStore.getState().citationIds.size).toBe(0);

    unmount();
  });

  it("does not re-fire for the same message id when the store updates other state", async () => {
    useChatStore.getState().setSessionMessages(SESSION, [
      makeImpactMessage("a1", {
        output: JSON.stringify({ key: "Foo#", impact: [{ key: "Bar#" }] }),
        success: true,
      }),
    ]);

    const { unmount } = mountHarvest();
    await flushMicrotasks();

    expect(fetchSnapshotMock).toHaveBeenCalledTimes(1);
    expect(useCodeGraphStore.getState().citationIds.size).toBe(2);

    // Touch unrelated slices (streaming, drafts) — the listener should
    // see the change, diff by id, and skip the work.
    act(() => {
      useChatStore.getState().appendStreamingText(SESSION, "noise");
      useChatStore.getState().setDraft(SESSION, "noise draft");
    });
    await flushMicrotasks();

    expect(fetchSnapshotMock).toHaveBeenCalledTimes(1);

    unmount();
  });

  it("re-fetches only when a NEW assistant message is appended", async () => {
    useChatStore.getState().setSessionMessages(SESSION, [
      makeImpactMessage("a1", {
        output: JSON.stringify({ key: "Foo#", impact: [{ key: "Bar#" }] }),
        success: true,
      }),
    ]);

    const { unmount } = mountHarvest();
    await flushMicrotasks();
    expect(fetchSnapshotMock).toHaveBeenCalledTimes(1);

    act(() => {
      useChatStore.getState().addMessage(SESSION, {
        id: "a2",
        role: "assistant",
        content: "more",
        createdAt: 99,
        toolCalls: [
          {
            name: "search",
            result: {
              output: JSON.stringify({
                hits: [{ key: "scip-rust crates lib 0.1.0 src/. qux()." }],
              }),
              success: true,
            },
          },
        ],
      });
    });
    await flushMicrotasks();

    expect(fetchSnapshotMock).toHaveBeenCalledTimes(2);
    const ids = useCodeGraphStore.getState().citationIds;
    expect(ids).toEqual(
      new Set([
        "scip-typescript npm pkg 1.0.0 src/Foo#",
        "scip-typescript npm pkg 1.0.0 src/Bar#",
        "scip-rust crates lib 0.1.0 src/. qux().",
      ]),
    );

    unmount();
  });

  it("passes the active project's slug into fetchSnapshot", async () => {
    useChatStore.getState().setSessionMessages(SESSION, [
      makeImpactMessage("a1", {
        output: JSON.stringify({ key: "Foo#", impact: [{ key: "Bar#" }] }),
        success: true,
      }),
    ]);

    const { unmount } = mountHarvest();
    await flushMicrotasks();

    expect(fetchSnapshotMock).toHaveBeenCalledWith(PROJECT, expect.any(Number));

    unmount();
  });
});
