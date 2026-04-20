import { beforeEach, afterEach, describe, expect, it, vi } from "vitest";
import { render, screen, userEvent, waitFor, within } from "@/test/test-utils";
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
    vi.mocked(callMcpTool).mockImplementation(async (toolName, args) => {
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
            {
              id: "adr-102",
              title: "Architectural draft",
              path: "/tmp/adr-102.md",
              work_shape: "architectural",
              originating_spike_id: "sp-102",
              mtime: "2026-04-09T09:00:00Z",
            },
          ],
        } as never;
      }

      if (toolName === "propose_adr_show") {
        const id = (args as { id?: string } | undefined)?.id;
        return {
          adr: {
            id: id ?? "adr-101",
            title: id === "adr-102" ? "Architectural draft" : "Proposal draft",
            path: `/tmp/${id ?? "adr-101"}.md`,
            work_shape: id === "adr-102" ? "architectural" : "epic",
            body: id === "adr-102" ? "# Architectural draft\n\nDecision body" : "# Proposal draft\n\nBody",
            originating_spike_id: id === "adr-102" ? "sp-102" : "sp-101",
            mtime: id === "adr-102" ? "2026-04-09T09:00:00Z" : "2026-04-09T10:00:00Z",
          },
        } as never;
      }

      if (toolName === "propose_adr_accept") {
        return {
          accepted_path: "/accepted/adr-101.md",
          epic: {
            id: "epic-1",
            short_id: "e1",
            title: "Proposal draft",
            description: "Epic shell",
            emoji: "🧠",
            color: "#fff",
            status: "open",
            owner: "fernando",
            created_at: "2026-04-09T12:00:00Z",
            updated_at: "2026-04-09T12:00:00Z",
            memory_refs: [],
            auto_breakdown: true,
          },
        } as never;
      }

      if (toolName === "propose_adr_reject") {
        return {
          ok: true,
          feedback_target: "task/sp-102",
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

  it("accepts a proposal from the loaded detail panel using scoped controls", async () => {
    const user = userEvent.setup();

    render(<ArchitectProposalsSection projectPath="/tmp/project" />);

    await user.click((await screen.findByText("Proposal draft")).closest("button") as HTMLButtonElement);

    const detailPanel = await screen.findByLabelText("Proposal detail panel");
    await within(detailPanel).findByText("Proposal ID");

    await user.click(within(detailPanel).getByRole("button", { name: "Accept" }));
    await user.click(within(detailPanel).getByRole("button", { name: "Confirm accept" }));

    await waitFor(() => {
      expect(callMcpTool).toHaveBeenCalledWith(
        "propose_adr_accept",
        expect.objectContaining({
          project: "/tmp/project",
          id: "adr-101",
          title: "Proposal draft",
          create_epic: true,
          auto_breakdown: true,
        }),
      );
    });
  });

  it("rejects a proposal from the loaded detail panel using scoped controls", async () => {
    const user = userEvent.setup();

    render(<ArchitectProposalsSection projectPath="/tmp/project" />);

    await user.click((await screen.findByText("Architectural draft")).closest("button") as HTMLButtonElement);

    const detailPanel = await screen.findByLabelText("Proposal detail panel");
    await within(detailPanel).findByText("Proposal ID");

    await user.click(within(detailPanel).getByRole("button", { name: "Reject" }));
    await user.type(within(detailPanel).getByLabelText("Reason"), "Needs more detail");
    await user.click(within(detailPanel).getByRole("button", { name: "Confirm reject" }));

    await waitFor(() => {
      expect(callMcpTool).toHaveBeenCalledWith(
        "propose_adr_reject",
        expect.objectContaining({
          project: "/tmp/project",
          id: "adr-102",
          reason: "Needs more detail",
        }),
      );
    });
  });
});
