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

vi.mock("@/components/userConfig/ModelSection", async () => {
  const { useQueryClient } = await import("@tanstack/react-query");
  return {
    ModelSection: ({
      targetId,
      onboarding,
    }: {
      targetId: string;
      onboarding?: boolean;
    }) => {
      const queryClient = useQueryClient();
      return (
        <div>
          <span>{onboarding ? "Onboarding model editor" : "Settings model editor"}</span>
          <button
            type="button"
            onClick={() =>
              queryClient.setQueryData(
                ["user-config", targetId, "model-selection"],
                {
                  lanes: {
                    plan: ["openai/gpt-5.5"],
                    implement: ["openai/gpt-5.3-codex"],
                    review: ["openai/gpt-5.5"],
                  },
                  maxSessions: {
                    "openai/gpt-5.5": 1,
                    "openai/gpt-5.3-codex": 1,
                  },
                  diverseReview: true,
                  diverseRefinement: true,
                  laneLocked: false,
                },
              )
            }
          >
            Save all roles
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

  it("keeps Continue disabled when persisted lanes are only partially configured", async () => {
    mocks.fetchUserModelSelection.mockResolvedValue({
      lanes: {
        plan: ["openai/gpt-5.5"],
        implement: ["openai/gpt-5.3-codex"],
        review: [],
      },
      maxSessions: {},
      diverseReview: true,
      diverseRefinement: true,
      laneLocked: false,
    });

    const user = userEvent.setup();
    render(<FirstRunOnboarding userId="user-1" onFinished={vi.fn()} />);

    await user.click(await screen.findByRole("button", { name: /continue/i }));

    expect(screen.getByText("Onboarding model editor")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /continue/i })).toBeDisabled();
  });

  it("enables Continue when all persisted model roles are configured", async () => {
    mocks.fetchUserModelSelection.mockResolvedValue({
      lanes: {
        plan: ["openai/gpt-5.5"],
        implement: ["openai/gpt-5.3-codex"],
        review: ["openai/gpt-5.5"],
      },
      maxSessions: {},
      diverseReview: true,
      diverseRefinement: true,
      laneLocked: false,
    });

    const user = userEvent.setup();
    render(<FirstRunOnboarding userId="user-1" onFinished={vi.fn()} />);

    await user.click(await screen.findByRole("button", { name: /continue/i }));

    expect(screen.getByRole("button", { name: /continue/i })).toBeEnabled();
  });

  it("requires persisted selections for Plan, Implement, and Review", async () => {
    const user = userEvent.setup();
    const onFinished = vi.fn();
    render(<FirstRunOnboarding userId="user-1" onFinished={onFinished} />);

    const connectContinue = await screen.findByRole("button", { name: /continue/i });
    await waitFor(() => expect(connectContinue).toBeEnabled());
    await user.click(connectContinue);

    expect(
      screen.getByRole("heading", { name: "Assign models to roles" }),
    ).toBeInTheDocument();
    const modelsContinue = screen.getByRole("button", { name: /continue/i });
    expect(modelsContinue).toBeDisabled();

    await user.click(screen.getByRole("button", { name: "Save all roles" }));
    await waitFor(() => expect(modelsContinue).toBeEnabled());
    await user.click(modelsContinue);

    expect(screen.getByRole("heading", { name: "You're all set" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Get started" }));
    expect(onFinished).toHaveBeenCalledOnce();
  });

  it("treats org-locked roles as complete and does not render the editable model section", async () => {
    mocks.fetchUserModelSelection.mockResolvedValue({
      lanes: { plan: [], implement: [], review: [] },
      maxSessions: {},
      diverseReview: true,
      diverseRefinement: true,
      laneLocked: true,
    });

    const user = userEvent.setup();
    render(<FirstRunOnboarding userId="user-1" onFinished={vi.fn()} />);

    await user.click(await screen.findByRole("button", { name: /continue/i }));

    expect(screen.getByText("Managed by your organization")).toBeInTheDocument();
    expect(screen.queryByText("Onboarding model editor")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: /continue/i })).toBeEnabled();
  });

  it("keeps the role step skippable for users relying on deployment fallback", async () => {
    const user = userEvent.setup();
    render(<FirstRunOnboarding userId="user-1" onFinished={vi.fn()} />);

    await user.click(await screen.findByRole("button", { name: /continue/i }));
    await user.click(screen.getByRole("button", { name: "Skip for now" }));

    expect(screen.getByRole("heading", { name: "You're all set" })).toBeInTheDocument();
  });
});
