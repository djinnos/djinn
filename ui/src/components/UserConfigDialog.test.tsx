import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor, within, render, userEvent } from "@/test/test-utils";
import { UserConfigDialog } from "@/components/UserConfigDialog";
import type { OrgUser } from "@/api/users";
import {
  fetchUserCatalog,
  fetchUserConnectedModels,
  fetchUserConnectedProviders,
  fetchUserModelSelection,
  saveUserModelSelection,
  setUserCredential,
  startUserOAuth,
  type UserModel,
  type CatalogProvider,
  type ConnectedProvider,
} from "@/api/userConfig";
import { showToast } from "@/lib/toast";

vi.mock("@/api/serverUrl", () => ({
  getServerBaseUrl: () => "http://djinn.test",
}));

vi.mock("@/lib/toast", () => ({
  showToast: {
    success: vi.fn(),
    error: vi.fn(),
    info: vi.fn(),
    warning: vi.fn(),
  },
}));

vi.mock("@/api/userConfig", () => ({
  fetchUserCatalog: vi.fn(),
  fetchUserConnectedProviders: vi.fn(),
  fetchUserConnectedModels: vi.fn(),
  fetchUserModelSelection: vi.fn(),
  saveUserModelSelection: vi.fn(),
  setUserCredential: vi.fn(),
  startUserOAuth: vi.fn(),
}));

class MockEventSource {
  static instances: MockEventSource[] = [];
  url: string;
  close = vi.fn();
  addEventListener = vi.fn();

  constructor(url: string) {
    this.url = url;
    MockEventSource.instances.push(this);
  }
}

const targetUser: OrgUser = {
  id: "target-user-1",
  github_login: "target-user",
  github_name: "Target User",
  github_avatar_url: null,
  is_member_of_org: true,
  is_admin: false,
  role: "engineer",
  last_seen_at: null,
};

const pricing = {
  cache_read_per_million: 0,
  cache_write_per_million: 0,
  input_per_million: 1,
  output_per_million: 5,
};

function provider(overrides: Partial<CatalogProvider> & Pick<CatalogProvider, "id" | "name">): CatalogProvider {
  return {
    base_url: "https://api.example.test",
    builtin_id: overrides.id,
    connected: false,
    connection_methods: [],
    docs_url: "https://docs.example.test",
    env_vars: [],
    goose_provider_id: overrides.id,
    is_openai_compatible: false,
    npm: "",
    oauth_keys: [],
    oauth_supported: false,
    ...overrides,
  };
}

function model(overrides: Partial<UserModel> & Pick<UserModel, "id" | "name" | "provider_id">): UserModel {
  return {
    attachment: false,
    context_window: 200_000,
    output_limit: 8_192,
    pricing,
    reasoning: true,
    tool_call: true,
    ...overrides,
  };
}

const catalog: CatalogProvider[] = [
  provider({
    id: "openai",
    name: "OpenAI",
    oauth_supported: true,
    oauth_keys: ["CHATGPT_CODEX"],
  }),
  provider({
    id: "anthropic",
    name: "Anthropic",
    env_vars: ["ANTHROPIC_API_KEY"],
  }),
];

const connectedProviders: ConnectedProvider[] = [
  provider({
    id: "openai",
    name: "OpenAI",
    connected: true,
    connection_methods: ["oauth"],
    oauth_supported: true,
  }) as ConnectedProvider,
];

const connectedModels: UserModel[] = [
  model({ id: "openai/gpt-5", name: "GPT-5", provider_id: "openai" }),
  model({ id: "anthropic/claude-opus-4-6", name: "Claude Opus 4.6", provider_id: "anthropic" }),
];

function mockSuccessfulLoads() {
  vi.mocked(fetchUserCatalog).mockResolvedValue(catalog);
  vi.mocked(fetchUserConnectedProviders).mockResolvedValue(connectedProviders);
  vi.mocked(fetchUserConnectedModels).mockResolvedValue(connectedModels);
  vi.mocked(fetchUserModelSelection).mockResolvedValue({
    lanes: { plan: ["openai/gpt-5"], implement: [], review: [] },
    maxSessions: { "openai/gpt-5": 3 },
    diverseReview: true,
  });
  vi.mocked(saveUserModelSelection).mockResolvedValue({
    lanes: { plan: ["openai/gpt-5"], implement: [], review: [] },
    maxSessions: { "openai/gpt-5": 3 },
    diverseReview: true,
  });
  vi.mocked(setUserCredential).mockResolvedValue(undefined);
  vi.mocked(startUserOAuth).mockResolvedValue({ kind: "connected" });
}

function renderDialog() {
  return render(
    <UserConfigDialog user={targetUser} open={true} onOpenChange={vi.fn()} />,
  );
}

describe("UserConfigDialog", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    MockEventSource.instances = [];
    vi.stubGlobal("EventSource", MockEventSource);
    mockSuccessfulLoads();
  });

  it("renders provider and model configuration for the target user", async () => {
    renderDialog();

    expect(screen.getByRole("heading", { name: /configure user/i })).toBeInTheDocument();
    expect(screen.getByText(/Target User/)).toBeInTheDocument();
    expect(screen.getByText(/Configure this user's providers/i)).toBeInTheDocument();

    expect(await screen.findByRole("button", { name: /reconnect/i })).toBeInTheDocument();
    expect(screen.getAllByText("OpenAI")).not.toHaveLength(0);
    expect(screen.getByText("Connected:")).toBeInTheDocument();
    expect(screen.getByText("ChatGPT / Codex")).toBeInTheDocument();

    expect(screen.getByLabelText(/provider/i)).toBeInTheDocument();
    // The Base UI Combobox renders options only when opened; we verify
    // the input and placeholder are present (option content is tested in
    // the "stores an API key" test below).
    expect(screen.getByLabelText(/provider/i)).toHaveAttribute("placeholder", "Select a provider…");
    expect(screen.getByLabelText(/api key/i)).toBeDisabled();
    expect(screen.getByText(/Stored encrypted and owned by this user/i)).toBeInTheDocument();

    expect(await screen.findByText("GPT-5")).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Model roles" })).toBeInTheDocument();
    expect(screen.getAllByText("OpenAI")).not.toHaveLength(0);
    expect(screen.getByText("Sessions:")).toBeInTheDocument();
    expect(screen.getByDisplayValue("3")).toBeInTheDocument();
    // One Add model trigger per lane (plan / implement / review).
    expect(screen.getAllByRole("button", { name: /add model/i })).not.toHaveLength(0);

    expect(fetchUserCatalog).toHaveBeenCalledWith(targetUser.id);
    expect(fetchUserConnectedProviders).toHaveBeenCalledWith(targetUser.id);
    expect(fetchUserConnectedModels).toHaveBeenCalledWith(targetUser.id);
    expect(fetchUserModelSelection).toHaveBeenCalledWith(targetUser.id);
  });

  it("stores an API key for the selected provider and resets the form", async () => {
    renderDialog();
    const user = userEvent.setup();

    const providerInput = await screen.findByLabelText(/provider/i);
    // Open the Base UI Combobox and select "Anthropic" by clicking.
    await user.click(providerInput);
    await user.click(await screen.findByText("Anthropic"));

    await waitFor(() => {
      expect(screen.getByLabelText(/api key \(ANTHROPIC_API_KEY\)/i)).not.toBeDisabled();
    });
    const apiKeyInput = screen.getByLabelText(/api key \(ANTHROPIC_API_KEY\)/i);
    await user.type(apiKeyInput, "  sk-ant-test-key  ");
    await user.click(screen.getByRole("button", { name: /^connect$/i }));

    await waitFor(() => {
      expect(setUserCredential).toHaveBeenCalledWith({
        targetUserId: targetUser.id,
        providerId: "anthropic",
        keyName: "ANTHROPIC_API_KEY",
        apiKey: "sk-ant-test-key",
      });
    });

    expect(showToast.success).toHaveBeenCalledWith("Provider connected", {
      description: "Anthropic key stored for this user.",
    });
    await waitFor(() => expect(providerInput).toHaveValue(""));
    expect(screen.getByLabelText(/^api key/i)).toHaveValue("");
  });

  it("starts ChatGPT OAuth and shows the device-code state", async () => {
    vi.mocked(fetchUserConnectedProviders).mockResolvedValue([]);
    vi.mocked(startUserOAuth).mockResolvedValue({
      kind: "pending",
      pending: {
        userCode: "ABCD-EFGH",
        verificationUri: "https://chatgpt.example/device",
        verificationUriComplete: "https://chatgpt.example/device?user_code=ABCD-EFGH",
        expiresInSecs: 600,
      },
    });

    renderDialog();
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: /continue with chatgpt/i }));

    await waitFor(() => {
      expect(startUserOAuth).toHaveBeenCalledWith(targetUser.id, "openai");
    });
    expect(screen.getByText(/Open the sign-in page and enter this code/i)).toBeInTheDocument();
    expect(screen.getByText("ABCD-EFGH")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /copy code/i })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /open sign-in page/i })).toHaveAttribute(
      "href",
      "https://chatgpt.example/device?user_code=ABCD-EFGH",
    );
    expect(screen.getByText(/Waiting for sign-in to complete \(expires in 10 min\)/i)).toBeInTheDocument();
    await waitFor(() => {
      expect(MockEventSource.instances[0]?.url).toBe("http://djinn.test/events");
    });
  });

  it("surfaces a server-reported ChatGPT OAuth failure inline", async () => {
    vi.mocked(fetchUserConnectedProviders).mockResolvedValue([]);
    vi.mocked(startUserOAuth).mockResolvedValue({
      kind: "error",
      message: "Could not persist Codex credentials",
    });

    renderDialog();
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: /continue with chatgpt/i }));

    expect(
      await screen.findByText("Could not persist Codex credentials"),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /try again/i })).toBeInTheDocument();
    expect(
      screen.queryByText(/waiting for sign-in to complete/i),
    ).not.toBeInTheDocument();
  });

  it("shows a friendly inline error when model selection fails to load", async () => {
    vi.mocked(fetchUserModelSelection).mockRejectedValue(
      new Error("Failed to load user settings"),
    );

    renderDialog();

    expect(await screen.findByText("Failed to load user settings")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /retry/i })).toBeInTheDocument();
    expect(screen.getByText("Providers")).toBeInTheDocument();
    expect(screen.getByText("Connect with an API key")).toBeInTheDocument();

    const modelsSection = screen.getByRole("heading", { name: "Model roles" }).closest("section");
    expect(modelsSection).not.toBeNull();
    expect(within(modelsSection as HTMLElement).getByText("Failed to load user settings")).toBeInTheDocument();
  });
});
