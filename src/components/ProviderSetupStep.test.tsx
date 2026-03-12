import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { ProviderSetupStep } from "@/components/ProviderSetupStep";
import { useWizardStore } from "@/stores/wizardStore";

vi.mock("@/api/server", () => ({
  fetchProviderCatalog: vi.fn(),
  validateProviderApiKey: vi.fn(),
  saveProviderCredentials: vi.fn(),
  startProviderOAuth: vi.fn(),
}));

import {
  fetchProviderCatalog,
  validateProviderApiKey,
  startProviderOAuth,
} from "@/api/server";

describe("ProviderSetupStep", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useWizardStore.getState().resetWizard();
    useWizardStore.setState({ totalSteps: 4, isCompleted: false, currentStep: 1 });
  });

  it("renders provider catalog and handles API key validation error", async () => {
    vi.mocked(fetchProviderCatalog).mockResolvedValue([
      {
        id: "openai",
        name: "OpenAI",
        requires_api_key: true,
        oauth_supported: false,
        description: "OpenAI provider",
      },
    ] as any);
    vi.mocked(validateProviderApiKey).mockResolvedValue({ valid: false, error: "Bad key" } as any);

    const { container } = render(<ProviderSetupStep />);

    expect(await screen.findByText("Configure AI Provider")).toBeInTheDocument();
    const dialog = container.firstElementChild as HTMLElement;
    fireEvent.click(within(dialog).getByRole("combobox"));
    fireEvent.click(await screen.findByRole("option", { name: /OpenAI/i }));

    const keyInput = await screen.findByPlaceholderText("Enter your API key");
    fireEvent.change(keyInput, { target: { value: "sk-test" } });
    fireEvent.click(screen.getByRole("button", { name: "Validate" }));

    await waitFor(() => {
      expect(validateProviderApiKey).toHaveBeenCalledWith("openai", "sk-test");
    });
    expect(await screen.findByText("Bad key")).toBeInTheDocument();
  });

  it("shows OAuth button and advances when OAuth succeeds", async () => {
    vi.mocked(fetchProviderCatalog).mockResolvedValue([
      {
        id: "github",
        name: "GitHub",
        requires_api_key: false,
        oauth_supported: true,
        description: "OAuth provider",
      },
    ] as any);
    vi.mocked(startProviderOAuth).mockResolvedValue({ success: true } as any);

    const { container } = render(<ProviderSetupStep />);

    expect(await screen.findByText("Configure AI Provider")).toBeInTheDocument();
    const dialog = container.firstElementChild as HTMLElement;
    fireEvent.click(within(dialog).getByRole("combobox"));
    fireEvent.click(await screen.findByRole("option", { name: /GitHub/i }));
    fireEvent.click(await screen.findByRole("button", { name: /Connect with OAuth/i }));

    await waitFor(() => expect(startProviderOAuth).toHaveBeenCalledWith("github"));
    await waitFor(() => expect(useWizardStore.getState().currentStep).toBe(2));
  });
});
