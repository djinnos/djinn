import { describe, expect, it } from "vitest";
import { buildLedger, toolDetail } from "./buildLedger";
import type { SessionInfo, TimelineEntry } from "@/hooks/useSessionMessages";

const session = (over: Partial<SessionInfo> & { id: string; agentType: string }): SessionInfo => ({
  modelId: "openai/gpt-5.6-sol",
  startedAt: "2026-08-04T18:30:00.000Z",
  status: "completed",
  tokensIn: 0,
  tokensOut: 0,
  cacheReadTokens: 0,
  cacheWriteTokens: 0,
  ...over,
});

const msg = (
  sessionId: string,
  agentType: string,
  content: Record<string, unknown>[],
): TimelineEntry => ({
  kind: "message",
  role: "assistant",
  content: content as TimelineEntry extends { content: infer C } ? C : never,
  sessionId,
  agentType,
  modelId: "openai/gpt-5.6-sol",
  timestamp: "2026-08-04T18:31:00.000Z",
});

describe("buildLedger", () => {
  it("keeps every non-final tool call as an activity step", () => {
    const { entries } = buildLedger({
      timeline: [
        msg("s1", "worker", [
          { type: "thinking", thinking: "**Inspecting branch commit diffs**" },
          { type: "tool_use", name: "shell", input: { command: "git diff --stat" } },
          { type: "tool_use", name: "Read", input: { file_path: "src/lib.rs" } },
          { type: "tool_use", name: "Grep", input: { pattern: "sse_frame_parser_v1" } },
        ]),
      ],
      sessions: [session({ id: "s1", agentType: "worker" })],
    });

    const phase = entries.find((e) => e.kind === "phase" && e.agentType === "worker");
    const strand = phase?.kind === "phase" ? phase.turns[0].blocks[0] : undefined;
    expect(strand?.kind).toBe("activity");
    if (strand?.kind !== "activity") throw new Error("expected activity strand");

    // The regression this whole view exists for: SessionThread dropped these.
    expect(strand.steps.filter((s) => s.kind === "tool")).toHaveLength(3);
    expect(strand.steps.map((s) => s.label)).toEqual(["Inspecting branch commit diffs", "shell", "Read", "Grep"]);
    expect(strand.steps[1].detail).toBe("git diff --stat");
  });

  it("reads provider reasoning that is not typed `thinking`", () => {
    const { entries } = buildLedger({
      timeline: [
        msg("s1", "reviewer", [
          {
            type: "open_a_i_reasoning",
            summary: [{ type: "summary_text", text: "**Inspecting provider test files**" }],
          },
        ]),
      ],
      sessions: [session({ id: "s1", agentType: "reviewer" })],
    });

    const phase = entries.find((e) => e.kind === "phase");
    const strand = phase?.kind === "phase" ? phase.turns[0].blocks[0] : undefined;
    if (strand?.kind !== "activity") throw new Error("expected activity strand");
    expect(strand.steps[0]).toMatchObject({
      kind: "thinking",
      label: "Inspecting provider test files",
    });
  });

  it("renders a submission as an artifact, not a strand step", () => {
    const { entries } = buildLedger({
      timeline: [
        msg("s1", "worker", [
          { type: "tool_use", name: "shell", input: { command: "cargo fmt" } },
          {
            type: "tool_use",
            name: "submit_work",
            input: {
              summary: "Recovered the covered B1 reply-loop launch path.",
              files_changed: ["a.rs", "b.rs"],
              remaining_concerns: ["scoped provider test command not run"],
            },
          },
        ]),
      ],
      sessions: [session({ id: "s1", agentType: "worker" })],
    });

    const phase = entries.find((e) => e.kind === "phase");
    if (phase?.kind !== "phase") throw new Error("expected phase");
    const [strand, artifact] = phase.turns[0].blocks;
    expect(strand.kind).toBe("activity");
    expect(artifact).toMatchObject({
      kind: "artifact",
      variant: "work_submitted",
      files: ["a.rs", "b.rs"],
      concerns: ["scoped provider test command not run"],
    });
  });

  it("splits phases on agent change and inserts a handoff", () => {
    const { entries } = buildLedger({
      timeline: [
        msg("s1", "worker", [{ type: "text", text: "done" }]),
        msg("s2", "reviewer", [{ type: "text", text: "reviewing" }]),
      ],
      sessions: [
        session({ id: "s1", agentType: "worker" }),
        session({ id: "s2", agentType: "reviewer" }),
      ],
    });

    expect(entries.map((e) => (e.kind === "handoff" ? `>${e.to}` : e.agentType))).toEqual([
      ">worker",
      "worker",
      ">reviewer",
      "reviewer",
    ]);
  });

  it("folds a respawned agent into one phase and counts the failures", () => {
    // The real shape of task 3j5q: one review phase, six reviewer sessions,
    // five of them dead. Previously all five rendered as nothing.
    const reviewerSessions = [
      session({ id: "r1", agentType: "reviewer", status: "failed" }),
      session({ id: "r2", agentType: "reviewer", status: "failed" }),
      session({ id: "r3", agentType: "reviewer", status: "failed" }),
      session({ id: "r4", agentType: "reviewer", status: "failed" }),
      session({ id: "r5", agentType: "reviewer", status: "failed" }),
      session({ id: "r6", agentType: "reviewer", status: "running" }),
    ];

    const { entries, agents } = buildLedger({
      timeline: [
        msg("r1", "reviewer", [{ type: "tool_use", name: "shell", input: { command: "git diff" } }]),
        msg("r6", "reviewer", [{ type: "tool_use", name: "shell", input: { command: "cargo test" } }]),
      ],
      sessions: reviewerSessions,
    });

    const phases = entries.filter((e) => e.kind === "phase" && e.agentType === "reviewer");
    expect(phases).toHaveLength(1);
    const phase = phases[0];
    if (phase.kind !== "phase") throw new Error("expected phase");
    expect(phase.attempts).toEqual({ total: 6, failed: 5 });
    expect(phase.running).toBe(true);
    expect(agents.find((a) => a.agentType === "reviewer")?.running).toBe(true);
  });

  it("maps criteria and the brief without inventing a phase", () => {
    const { entries, criteria } = buildLedger({
      timeline: [],
      sessions: [],
      description: "Recover the covered launch cut.",
      criteria: [
        { criterion: "fixtures land", met: true },
        { criterion: "cargo test passes", met: false },
      ],
      filedBy: "fernando",
    });

    expect(entries).toHaveLength(1);
    expect(entries[0].kind === "phase" && entries[0].brief?.body).toBe(
      "Recover the covered launch cut.",
    );
    expect(criteria).toEqual([
      { text: "fixtures land", met: true },
      { text: "cargo test passes", met: false },
    ]);
  });

  it("drops user messages", () => {
    const { entries } = buildLedger({
      timeline: [
        { ...msg("s1", "worker", [{ type: "text", text: "prompt" }]), role: "user" } as TimelineEntry,
      ],
      sessions: [session({ id: "s1", agentType: "worker" })],
    });
    expect(entries.filter((e) => e.kind === "phase")).toHaveLength(0);
  });
});

describe("toolDetail", () => {
  it("prefers the conventional subject key", () => {
    expect(toolDetail({ command: "git diff", cwd: "/x" })).toBe("git diff");
    expect(toolDetail({ file_path: "src/lib.rs" })).toBe("src/lib.rs");
    expect(toolDetail({ pattern: "foo" })).toBe("foo");
  });

  it("falls back to truncated json", () => {
    const detail = toolDetail({ weird: "x".repeat(400) });
    expect(detail.length).toBeLessThanOrEqual(120);
    expect(detail.endsWith("…")).toBe(true);
  });
});
