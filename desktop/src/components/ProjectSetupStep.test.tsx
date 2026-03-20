import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { ProjectSetupStep } from "@/components/ProjectSetupStep";
import { useWizardStore } from "@/stores/wizardStore";

vi.mock("@/api/server", () => ({
  addProject: vi.fn(),
}));

vi.mock("@/tauri/commands", () => ({
  selectDirectory: vi.fn(),
}));

import { addProject } from "@/api/server";
import { selectDirectory } from "@/tauri/commands";

describe("ProjectSetupStep", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useWizardStore.getState().resetWizard();
    useWizardStore.setState({ totalSteps: 4, isCompleted: false, currentStep: 2 });
  });

  it("renders input, submits project, and shows errors", async () => {
    vi.mocked(selectDirectory).mockResolvedValue("/tmp/my-project");
    vi.mocked(addProject).mockRejectedValue(new Error("add failed"));

    render(<ProjectSetupStep />);
    expect(screen.getByText("Set Up Your Project")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: /Browse/i }));
    await waitFor(() => expect(selectDirectory).toHaveBeenCalledWith("Select Project Directory"));

    expect(await screen.findByDisplayValue("/tmp/my-project")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /Register Project/i }));

    await waitFor(() => expect(addProject).toHaveBeenCalledWith("/tmp/my-project"));
    expect(await screen.findByText("add failed")).toBeInTheDocument();
  });
});
