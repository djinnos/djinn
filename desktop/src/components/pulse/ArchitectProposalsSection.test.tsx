import { beforeEach, afterEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@/test/test-utils";
import { ArchitectProposalsSection } from "@/components/pulse/ArchitectProposalsSection";
import { callMcpTool } from "@/api/mcpClient";

vi.mock("@/api/mcpClient", () => ({
  callMcpTool: vi.fn(),
}));

vi.mock("@/lib/toast", () => ({
  showToast: { success: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

describe("ArchitectProposalsSection", () => {
  beforeEach(() => {
    vi.spyOn(Date, "now").mockReturnValue(new Date("2026-04-09T12:00:00Z").getTime());
    vi.mocked(callMcpTool).mockReset();
    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === "propose_adr_list") {
        return {
          items: [
            {
              id: "adr-101",
              title: "Proposal draft",
              path: "/tmp/adr-101.md",
              work_shape: "epic",
              mtime: "2026-04-09T10:00:00Z",
            },
          ],
        } as never;
      }

      if (toolName === "propose_adr_show") {
        return {
          adr: {
            id: "adr-101",
            title: "Proposal draft",
            path: "/tmp/adr-101.md",
            work_shape: "epic",
            body: "# Proposal draft\n\nBody",
            mtime: "2026-04-09T10:00:00Z",
          },
        } as never;
      }

      return {} as never;
    });
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("renders proposal age from mtime in the list and detail panel", async () => {
    render(<ArchitectProposalsSection projectPath="/tmp/project" />);

    expect(await screen.findByText("2h ago")).toBeInTheDocument();
    expect(await screen.findByText("Updated 2h ago")).toBeInTheDocument();
  });
});
