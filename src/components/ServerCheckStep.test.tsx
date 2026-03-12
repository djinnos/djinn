import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { ServerCheckStep } from "@/components/ServerCheckStep";

vi.mock("@/api/server", () => ({
  checkServerHealth: vi.fn(),
}));

import { checkServerHealth } from "@/api/server";

describe("ServerCheckStep", () => {
  beforeEach(() => {
    vi.useRealTimers();
    vi.clearAllMocks();
  });

  it("shows loading then success state", async () => {
    vi.mocked(checkServerHealth).mockResolvedValue({ status: "ok" });

    const { container } = render(<ServerCheckStep />);
    const stepRoot = container.firstElementChild as HTMLElement;
    expect(within(stepRoot).getByText("Connecting to Server")).toBeInTheDocument();

    await waitFor(() => expect(checkServerHealth).toHaveBeenCalledTimes(1));
    expect(await within(stepRoot).findByRole("heading", { name: "Server Connected" })).toBeInTheDocument();
  });

  it("shows error state and retry", async () => {
    vi.mocked(checkServerHealth)
      .mockRejectedValueOnce(new Error("down"))
      .mockResolvedValueOnce({ status: "ok" });

    const { container } = render(<ServerCheckStep />);
    const stepRoot = container.firstElementChild as HTMLElement;

    expect(await within(stepRoot).findByRole("heading", { name: "Connection Failed" })).toBeInTheDocument();
    expect(within(stepRoot).getByText("down")).toBeInTheDocument();

    fireEvent.click(within(stepRoot).getByRole("button", { name: /Retry Connection/i }));

    await waitFor(() => expect(checkServerHealth).toHaveBeenCalledTimes(2));
    expect(await within(stepRoot).findByRole("heading", { name: "Server Connected" })).toBeInTheDocument();
  });
});
