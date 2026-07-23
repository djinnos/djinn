import { callMcpTool } from "@/api/mcpClient";
import { proposalDetailQueryOptions } from "./proposalQueries";

vi.mock("@/api/mcpClient", () => ({
  callMcpTool: vi.fn(),
}));

async function fetchProposalDetail() {
  const queryFn = proposalDetailQueryOptions("proposal-1").queryFn;
  if (!queryFn) throw new Error("proposal detail query function is missing");
  return queryFn({} as never);
}

describe("proposalDetailQueryOptions", () => {
  beforeEach(() => {
    vi.mocked(callMcpTool).mockReset();
  });

  it("preserves populated head and immutable revision lint payloads", async () => {
    const latestLint = {
      body_format: "markdown",
      body_sha256: "head-sha",
      checked_at: "2026-07-23T00:00:00Z",
      errors: [],
      linter_version: "v1",
      skipped_tiers: [{ tier: "mdx", reason: "legacy_markdown" }],
      warnings: [{
        code: "FUTURE_SERVER_WARNING",
        message: "Opaque server diagnostic",
        severity: "warning",
        span: { start: 3, end: 9 },
      }],
    };
    const revisionLint = {
      ...latestLint,
      body_sha256: "revision-sha",
      warnings: [],
      errors: [{
        code: "FUTURE_SERVER_ERROR",
        message: "Another opaque server diagnostic",
        severity: "error",
        span: { start: 0, end: 2 },
      }],
    };
    const revisions = [{ id: "revision-1", seq: 1, lint: revisionLint }];

    vi.mocked(callMcpTool).mockResolvedValue({ latest_lint: latestLint, revisions } as never);

    const detail = await fetchProposalDetail();

    expect(detail.latest_lint).toBe(latestLint);
    expect(detail.revisions).toBe(revisions);
    expect(detail.revisions[0].lint).toBe(revisionLint);
    expect(detail.revisions[0].lint?.errors[0]?.code).toBe("FUTURE_SERVER_ERROR");
    expect(callMcpTool).toHaveBeenCalledWith("proposal_show", { id: "proposal-1" });
  });

  it("preserves explicit null head and revision lint values", async () => {
    const revisions = [{ id: "revision-1", seq: 1, lint: null }];
    vi.mocked(callMcpTool).mockResolvedValue({ latest_lint: null, revisions } as never);

    const detail = await fetchProposalDetail();

    expect(detail.latest_lint).toBeNull();
    expect(detail.revisions).toBe(revisions);
    expect(detail.revisions[0].lint).toBeNull();
  });

  it("leaves additive lint fields absent for legacy responses", async () => {
    const revisions = [{ id: "revision-1", seq: 1 }];
    vi.mocked(callMcpTool).mockResolvedValue({ revisions } as never);

    const detail = await fetchProposalDetail();

    expect(detail.latest_lint).toBeUndefined();
    expect(detail.revisions).toBe(revisions);
    expect(detail.revisions[0].lint).toBeUndefined();
  });
});
