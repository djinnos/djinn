import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor, render, userEvent } from "@/test/test-utils";

const mocks = vi.hoisted(() => ({
  fetchUserCatalog: vi.fn(),
  fetchUserConnectedProviders: vi.fn(),
  fetchUserModelSelection: vi.fn(),
}));

vi.mock("@/api/userConfig", () => ({
  SELF_TARGET: "__self__",
  fetchUserCatalog: mocks.fetchUserCatalog,
  fetchUserConnectedProviders: mocks.fetchUserConnectedProviders,
  fetchUserModelSelection: mocks.fetchUserModelSelection,
}));

vi.mock("@/components/userConfig/ProviderSection", () => ({
  CodexConnectCard: () => <div>Codex connection</div>,
  ApiKeyConnectForm: () => <div>API key connection</div>,
}));

vi.mock("./OnboardingModelSetup", () => {
  return {
    OnboardingModelSetup: ({
      onSaved,
    }: {
      onSaved: (selection: unknown) => void;
    }) => {
      return (
        <div>
          <span>Onboarding model setup</span>
          <button
            type="button"
            onClick={() => onSaved({})}
          >
            Save models and continue
          </button>
        </div>
      );
    },
  };
});

import { FirstRunOnboarding } from "./FirstRunOnboarding";

describe("FirstRunOnboarding", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    window.localStorage.clear();
    mocks.fetchUserCatalog.mockResolvedValue([]);
    mocks.fetchUserConnectedProviders.mockResolvedValue([
      {
        id: "openai",
        name: "OpenAI",
        connected: true,
        connection_methods: ["oauth"],
      },
    ]);
    mocks.fetchUserModelSelection.mockResolvedValue({
      lanes: { plan: [], implement: [], review: [] },
      maxSessions: {},
      diverseReview: true,
      diverseRefinement: true,
    });
  });

  it("requires a provider and offers no onboarding bypass", async () => {
    mocks.fetchUserConnectedProviders.mockResolvedValue([]);

    render(<FirstRunOnboarding onFinished={vi.fn()} />);

    const heading = await screen.findByRole("heading", {
      name: "Connect a model provider",
      level: 1,
    });
    expect(heading).toBeInTheDocument();
    await waitFor(() => expect(heading).toHaveFocus());
    expect(screen.queryByRole("button", { name: "Skip for now" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: /continue/i })).toBeDisabled();
  });

  it("surfaces catalog failures and retries both provider setup queries", async () => {
    mocks.fetchUserConnectedProviders.mockResolvedValue([]);
    mocks.fetchUserCatalog.mockRejectedValue(new Error("Catalog unavailable"));
    const user = userEvent.setup();

    render(<FirstRunOnboarding onFinished={vi.fn()} />);

    expect(await screen.findByText("Catalog unavailable")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Retry" }));

    await waitFor(() => {
      expect(mocks.fetchUserConnectedProviders).toHaveBeenCalledTimes(2);
      expect(mocks.fetchUserCatalog).toHaveBeenCalledTimes(2);
    });
  });

  it("resumes at focused role setup and finishes immediately after it saves", async () => {
    const user = userEvent.setup();
    const onFinished = vi.fn();
    render(<FirstRunOnboarding onFinished={onFinished} />);

    expect(await screen.findByText("Onboarding model setup")).toBeInTheDocument();
    const heading = screen.getByRole("heading", {
      name: "Assign models to roles",
      level: 1,
    });
    await waitFor(() => expect(heading).toHaveFocus());
    expect(screen.getByRole("status")).toHaveTextContent(
      "Step 2 of 3: Models",
    );
    expect(screen.queryByRole("button", { name: "Skip for now" })).not.toBeInTheDocument();
    await user.click(
      screen.getByRole("button", { name: "Save models and continue" }),
    );

    await waitFor(() => expect(onFinished).toHaveBeenCalledOnce());
  });

  it("treats complete org-locked roles as read-only success", async () => {
    mocks.fetchUserModelSelection.mockResolvedValue({
      lanes: {
        plan: ["openai/gpt-5.5"],
        implement: ["openai/gpt-5.3-codex"],
        review: ["openai/gpt-5.5"],
      },
      maxSessions: {},
      diverseReview: true,
      diverseRefinement: true,
      laneLocked: true,
    });

    render(<FirstRunOnboarding onFinished={vi.fn()} />);

    expect(await screen.findByText("Managed by your organization")).toBeInTheDocument();
    expect(screen.queryByText("Onboarding model setup")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: /continue/i })).toBeEnabled();
  });

  it("blocks org-locked users when the policy has no role assignments", async () => {
    mocks.fetchUserModelSelection.mockResolvedValue({
      lanes: { plan: [], implement: [], review: [] },
      maxSessions: {},
      diverseReview: true,
      diverseRefinement: true,
      laneLocked: true,
    });

    render(<FirstRunOnboarding onFinished={vi.fn()} />);

    expect(await screen.findByText("Model roles need an administrator")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /continue/i })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Skip for now" })).not.toBeInTheDocument();
  });
});
