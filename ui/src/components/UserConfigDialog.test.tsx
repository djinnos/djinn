import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor, within, render, userEvent } from "@/test/test-utils";
import { UserConfigDialog } from "@/components/UserConfigDialog";
import type { OrgUser } from "@/api/users";
import {
  fetchAutomationCatalog,
  fetchAutomationConnectedModels,
  fetchAutomationConnectedProviders,
  fetchAutomationModelSelection,
  saveAutomationModelSelection,
  setAutomationCredential,
  startAutomationOAuth,
  type AutomationModel,
  type CatalogProvider,
  type ConnectedProvider,
} from "@/api/automationConfig";
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

vi.mock("@/api/automationConfig", () => ({
  fetchAutomationCatalog: vi.fn(),
  fetchAutomationConnectedProviders: vi.fn(),
  fetchAutomationConnectedModels: vi.fn(),
  fetchAutomationModelSelection: vi.fn(),
  saveAutomationModelSelection: vi.fn(),
  setAutomationCredential: vi.fn(),
  startAutomationOAuth: vi.fn(),
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

const automationUser: OrgUser = {
  id: "automation-user-1",
  github_login: "djinn-automation",
  github_name: "Djinn Automation",
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

function model(overrides: Partial<AutomationModel> & Pick<AutomationModel, "id" | "name" | "provider_id">): AutomationModel {
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

const connectedModels: AutomationModel[] = [
  model({ id: "openai/gpt-5", name: "GPT-5", provider_id: "openai" }),
  model({ id: "anthropic/claude-opus-4-6", name: "Claude Opus 4.6", provider_id: "anthropic" }),
];

function mockSuccessfulLoads() {
  vi.mocked(fetchAutomationCatalog).mockResolvedValue(catalog);
  vi.mocked(fetchAutomationConnectedProviders).mockResolvedValue(connectedProviders);
  vi.mocked(fetchAutomationConnectedModels).mockResolvedValue(connectedModels);
  vi.mocked(fetchAutomationModelSelection).mockResolvedValue({
    models: ["openai/gpt-5"],
    maxSessions: { "openai/gpt-5": 3 },
  });
  vi.mocked(saveAutomationModelSelection).mockResolvedValue({
    models: ["openai/gpt-5"],
    maxSessions: { "openai/gpt-5": 3 },
  });
  vi.mocked(setAutomationCredential).mockResolvedValue(undefined);
  vi.mocked(startAutomationOAuth).mockResolvedValue({ kind: "connected" });
}

function renderDialog() {
  return render(
    <UserConfigDialog user={automationUser} open={true} onOpenChange={vi.fn()} />,
  );
}

describe("UserConfigDialog", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    MockEventSource.instances = [];
    vi.stubGlobal("EventSource", MockEventSource);
    mockSuccessfulLoads();
  });

  it("renders provider and model configuration for the automation user", async () => {
    renderDialog();

    expect(screen.getByRole("heading", { name: /configure automation/i })).toBeInTheDocument();
    expect(screen.getByText(/Djinn Automation/)).toBeInTheDocument();
    expect(screen.getByText(/can't sign in, so you configure it here/i)).toBeInTheDocument();

    expect(await screen.findByRole("button", { name: /reconnect/i })).toBeInTheDocument();
    expect(screen.getAllByText("OpenAI")).not.toHaveLength(0);
    expect(screen.getByText("Connected:")).toBeInTheDocument();
    expect(screen.getByText("ChatGPT / Codex")).toBeInTheDocument();

    expect(screen.getByLabelText(/provider/i)).toBeInTheDocument();
    expect(await screen.findByRole("option", { name: "Anthropic" })).toBeInTheDocument();
    expect(screen.getByLabelText(/api key/i)).toBeDisabled();
    expect(screen.getByText(/Stored encrypted and owned by the automation user/i)).toBeInTheDocument();

    expect(await screen.findByText("GPT-5")).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Models" })).toBeInTheDocument();
    expect(screen.getAllByText("OpenAI")).not.toHaveLength(0);
    expect(screen.getByText("Sessions:")).toBeInTheDocument();
    expect(screen.getByDisplayValue("3")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /add model/i })).toBeInTheDocument();

    expect(fetchAutomationCatalog).toHaveBeenCalledWith(automationUser.id);
    expect(fetchAutomationConnectedProviders).toHaveBeenCalledWith(automationUser.id);
    expect(fetchAutomationConnectedModels).toHaveBeenCalledWith(automationUser.id);
    expect(fetchAutomationModelSelection).toHaveBeenCalledWith(automationUser.id);
  });

  it("stores an API key for the selected provider and resets the form", async () => {
    renderDialog();
    const user = userEvent.setup();

    const providerSelect = await screen.findByLabelText(/provider/i);
    await screen.findByRole("option", { name: "Anthropic" });
    await user.selectOptions(providerSelect, "anthropic");

    const apiKeyInput = screen.getByLabelText(/api key \(ANTHROPIC_API_KEY\)/i);
    await user.type(apiKeyInput, "  sk-ant-test-key  ");
    await user.click(screen.getByRole("button", { name: /^connect$/i }));

    await waitFor(() => {
      expect(setAutomationCredential).toHaveBeenCalledWith({
        targetUserId: automationUser.id,
        providerId: "anthropic",
        keyName: "ANTHROPIC_API_KEY",
        apiKey: "sk-ant-test-key",
      });
    });

    expect(showToast.success).toHaveBeenCalledWith("Provider connected", {
      description: "Anthropic key stored for automation.",
    });
    await waitFor(() => expect(providerSelect).toHaveValue(""));
    expect(screen.getByLabelText(/^api key/i)).toHaveValue("");
  });

  it("starts ChatGPT OAuth and shows the device-code state", async () => {
    vi.mocked(fetchAutomationConnectedProviders).mockResolvedValue([]);
    vi.mocked(startAutomationOAuth).mockResolvedValue({
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
      expect(startAutomationOAuth).toHaveBeenCalledWith(automationUser.id, "openai");
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

  it("shows a friendly inline error when model selection fails to load", async () => {
    vi.mocked(fetchAutomationModelSelection).mockRejectedValue(
      new Error("Failed to load automation settings"),
    );

    renderDialog();

    expect(await screen.findByText("Failed to load automation settings")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /retry/i })).toBeInTheDocument();
    expect(screen.getByText("Providers")).toBeInTheDocument();
    expect(screen.getByText("Connect with an API key")).toBeInTheDocument();

    const modelsSection = screen.getByRole("heading", { name: "Models" }).closest("section");
    expect(modelsSection).not.toBeNull();
    expect(within(modelsSection as HTMLElement).getByText("Failed to load automation settings")).toBeInTheDocument();
  });
});
