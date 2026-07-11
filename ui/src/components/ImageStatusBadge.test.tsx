import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, userEvent, waitFor } from "@/test/test-utils";
import { ImageStatusBadge } from "./ImageStatusBadge";
import { deriveState } from "./imageStatusBadge.helpers";
import {
  fetchDevcontainerStatus,
  retriggerImageBuild,
  type DevcontainerStatus,
} from "@/api/devcontainer";

vi.mock("@/api/devcontainer", async (importOriginal) => {
  const actual =
    await importOriginal<typeof import("@/api/devcontainer")>();
  return {
    ...actual,
    fetchDevcontainerStatus: vi.fn(),
    retriggerImageBuild: vi.fn(),
  };
});

const status = (overrides: Partial<DevcontainerStatus>): DevcontainerStatus => ({
  image_status: "ready",
  needs_image: false,
  graph_warm_status: "ready",
  ...overrides,
});

describe("deriveState", () => {
  it("maps a ready image + degraded warm to the degraded state", () => {
    const state = deriveState(
      status({ image_status: "ready", graph_warm_status: "degraded" }),
    );
    expect(state.kind).toBe("degraded");
  });

  it("keeps the ready state when every workspace warmed cleanly", () => {
    expect(
      deriveState(status({ image_status: "ready", graph_warm_status: "ready" }))
        .kind,
    ).toBe("ready");
  });

  it("still routes non-degraded, non-ready warm to warming", () => {
    expect(
      deriveState(
        status({ image_status: "ready", graph_warm_status: "running" }),
      ).kind,
    ).toBe("warming");
  });
});

describe("ImageStatusBadge", () => {
  beforeEach(() => {
    vi.mocked(retriggerImageBuild).mockReset();
    vi.mocked(fetchDevcontainerStatus).mockReset();
  });

  it("renders the degraded badge and lists the failing workspaces", async () => {
    vi.mocked(fetchDevcontainerStatus).mockResolvedValue(
      status({
        image_status: "ready",
        graph_warm_status: "degraded",
        graph_warmed_at: "2026-07-11T00:00:00Z",
        workspace_warm_statuses: [
          { workspace_slug: "ui", status: "ready" },
          { workspace_slug: "server", status: "timed_out", commit_sha: "abc" },
        ],
      }),
    );

    render(<ImageStatusBadge projectId="p1" projectName="djinn" />);

    // The pill surfaces the degraded label (not a masked "Warming").
    const trigger = await screen.findByText("Graph degraded");
    await userEvent.click(trigger);

    await waitFor(() =>
      expect(screen.getByText(/Code graph degraded/i)).toBeInTheDocument(),
    );
    // The failing workspace is listed; the healthy one is not.
    expect(screen.getByText("server (timed_out)")).toBeInTheDocument();
    expect(screen.queryByText(/^ui \(/)).not.toBeInTheDocument();
  });
});
