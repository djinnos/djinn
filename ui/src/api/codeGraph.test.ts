import { beforeEach, describe, expect, it, vi } from "vitest";

import { callMcpTool } from "@/api/mcpClient";
import {
  callCodeGraph,
  fetchCoverage,
  fetchSnapshot,
  parseCoverageResponse,
  parseWorkspacesResponse,
} from "./codeGraph";

vi.mock("@/api/mcpClient", () => ({
  callMcpTool: vi.fn(),
}));

beforeEach(() => {
  vi.mocked(callMcpTool).mockReset();
  vi.mocked(callMcpTool).mockResolvedValue({} as never);
});

describe("parseWorkspacesResponse", () => {
  it("normalizes workspaces from the tagged MCP response", () => {
    expect(
      parseWorkspacesResponse({
        workspaces: [
          {
            slug: "api",
            display_name: "API",
            root_path: "server/api",
            language: "rust",
            status: "warm",
          },
        ],
      }),
    ).toEqual([
      {
        slug: "api",
        display: "API",
        root: "server/api",
        language: "rust",
        status: "warm",
      },
    ]);
  });

  it("accepts alternate wire field names and trims non-empty strings", () => {
    expect(
      parseWorkspacesResponse([
        {
          workspace_slug: " web ",
          label: " Web ",
          path: " ui ",
          indexer: "typescript",
          warm_status: "pending",
        },
      ]),
    ).toEqual([
      {
        slug: "web",
        display: "Web",
        root: "ui",
        language: "typescript",
        status: "pending",
      },
    ]);
  });

  it("drops entries without a non-empty slug", () => {
    expect(
      parseWorkspacesResponse({
        workspaces: [
          { slug: "" },
          { workspace_slug: "   " },
          { slug: 42 },
          { slug: "api" },
        ],
      }),
    ).toEqual([
      {
        slug: "api",
        display: undefined,
        root: undefined,
        language: undefined,
        status: undefined,
      },
    ]);
  });
});

describe("parseCoverageResponse", () => {
  it("extracts gaps from the coverage op payload", () => {
    const cov = parseCoverageResponse({
      coverage: true,
      has_gaps: true,
      workspaces: [
        { workspace_slug: "ui", language: "typescript", status: "indexed", is_gap: false },
        { workspace_slug: "server", language: "rust", status: "timed_out", is_gap: true },
      ],
      unindexed_source_roots: [
        {
          workspace_slug: "server",
          language: "rust",
          status: "timed_out",
          detail: "rust-analyzer timed out",
        },
      ],
    });

    expect(cov.hasGaps).toBe(true);
    expect(cov.gaps).toHaveLength(1);
    expect(cov.gaps[0]).toMatchObject({
      slug: "server",
      language: "rust",
      status: "timed_out",
    });
  });

  it("reports no gaps for a clean project", () => {
    const cov = parseCoverageResponse({
      coverage: true,
      has_gaps: false,
      workspaces: [
        { workspace_slug: "server", language: "rust", status: "indexed", is_gap: false },
      ],
      unindexed_source_roots: [],
    });
    expect(cov.hasGaps).toBe(false);
    expect(cov.gaps).toEqual([]);
  });

  it("degrades safely on a malformed payload", () => {
    expect(parseCoverageResponse(null)).toEqual({ hasGaps: false, gaps: [] });
    expect(parseCoverageResponse("nope")).toEqual({ hasGaps: false, gaps: [] });
  });
});

describe("fetchCoverage", () => {
  it("dispatches the coverage op and parses the result", async () => {
    vi.mocked(callMcpTool).mockResolvedValueOnce({
      coverage: true,
      has_gaps: true,
      unindexed_source_roots: [
        { workspace_slug: "server", language: "rust", status: "timed_out" },
      ],
    } as never);

    const cov = await fetchCoverage("project-a");

    expect(vi.mocked(callMcpTool)).toHaveBeenCalledWith("code_graph", {
      project: "project-a",
      operation: "coverage",
    });
    expect(cov.hasGaps).toBe(true);
    expect(cov.gaps[0].slug).toBe("server");
  });
});

describe("callCodeGraph snapshot payload", () => {
  it("forwards level: community to the server", async () => {
    await callCodeGraph("project-a", "snapshot", { level: "community" });

    expect(vi.mocked(callMcpTool)).toHaveBeenCalledWith("code_graph", {
      project: "project-a",
      operation: "snapshot",
      level: "community",
    });
  });

  it("forwards level: symbol to the server", async () => {
    await callCodeGraph("project-a", "snapshot", { level: "symbol" });

    expect(vi.mocked(callMcpTool)).toHaveBeenCalledWith("code_graph", {
      project: "project-a",
      operation: "snapshot",
      level: "symbol",
    });
  });
});

describe("fetchSnapshot level forwarding", () => {
  it("omits level when not provided (preserves server default)", async () => {
    await fetchSnapshot("project-a");

    const [, args] = vi.mocked(callMcpTool).mock.calls[0];
    expect(args).toEqual({
      project: "project-a",
      operation: "snapshot",
    });
    expect(args).not.toHaveProperty("level");
    expect(args).not.toHaveProperty("limit");
  });

  it("forwards nodeCap as limit while omitting level", async () => {
    await fetchSnapshot("project-a", 5_000);

    const [, args] = vi.mocked(callMcpTool).mock.calls[0];
    expect(args).toEqual({
      project: "project-a",
      operation: "snapshot",
      limit: 5_000,
    });
    expect(args).not.toHaveProperty("level");
  });

  it("forwards level: community alongside nodeCap", async () => {
    await fetchSnapshot("project-a", 5_000, "community");

    const [, args] = vi.mocked(callMcpTool).mock.calls[0];
    expect(args).toEqual({
      project: "project-a",
      operation: "snapshot",
      limit: 5_000,
      level: "community",
    });
  });

  it("forwards level: symbol without nodeCap", async () => {
    await fetchSnapshot("project-a", undefined, "symbol");

    const [, args] = vi.mocked(callMcpTool).mock.calls[0];
    expect(args).toEqual({
      project: "project-a",
      operation: "snapshot",
      level: "symbol",
    });
    expect(args).not.toHaveProperty("limit");
  });
});
