import { describe, expect, it, vi } from "vitest";

import type { ConnectedProvider } from "@/api/userConfig";
import { render, screen } from "@/test/test-utils";

import { ConnectionsTab } from "./ConnectionsTab";

vi.mock("@/lib/toast", () => ({
  showToast: { success: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

const connected: ConnectedProvider[] = [
  // Codex sub (oauth on openai) — rendered by its own compact row, not a
  // generic subscription card.
  {
    id: "openai",
    name: "OpenAI",
    connection_methods: ["oauth"],
    env_vars: ["OPENAI_API_KEY"],
  } as ConnectedProvider,
  // A personal subscription connected by API key (standalone, own key).
  {
    id: "kimi-for-coding",
    name: "Kimi for Coding",
    connection_methods: ["credential"],
    env_vars: ["KIMI_API_KEY"],
  } as ConnectedProvider,
  // Four Xiaomi endpoints that share ONE credential — must collapse to a
  // single "Xiaomi" account card, not four rows.
  {
    id: "xiaomi",
    name: "Xiaomi",
    connection_methods: ["credential"],
    env_vars: ["XIAOMI_API_KEY"],
  } as ConnectedProvider,
  {
    id: "xiaomi-token-plan-cn",
    name: "Xiaomi Token Plan (China)",
    connection_methods: ["credential"],
    env_vars: ["XIAOMI_API_KEY"],
  } as ConnectedProvider,
  {
    id: "xiaomi-token-plan-ams",
    name: "Xiaomi Token Plan (Europe)",
    connection_methods: ["credential"],
    env_vars: ["XIAOMI_API_KEY"],
  } as ConnectedProvider,
  {
    id: "xiaomi-token-plan-sgp",
    name: "Xiaomi Token Plan (Singapore)",
    connection_methods: ["credential"],
    env_vars: ["XIAOMI_API_KEY"],
  } as ConnectedProvider,
  // Z.AI + Zhipu coding plans share ZHIPU_API_KEY — one "Z.AI / Zhipu" card.
  {
    id: "zai-coding-plan",
    name: "Z.AI Coding Plan",
    connection_methods: ["credential"],
    env_vars: ["ZHIPU_API_KEY"],
  } as ConnectedProvider,
  {
    id: "zhipuai",
    name: "Zhipu AI",
    connection_methods: ["credential"],
    env_vars: ["ZHIPU_API_KEY"],
  } as ConnectedProvider,
  // An org-provided API key.
  {
    id: "anthropic",
    name: "Anthropic",
    connection_methods: ["credential"],
    env_vars: ["ANTHROPIC_API_KEY"],
  } as ConnectedProvider,
];

vi.mock("@/api/userConfig", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/api/userConfig")>();
  return {
    ...actual,
    fetchUserConnectedProviders: vi.fn(async () => connected),
    fetchUserCatalog: vi.fn(async () => []),
  };
});

describe("ConnectionsTab", () => {
  it("splits providers into buckets and collapses same-account subscriptions", async () => {
    render(<ConnectionsTab />);

    // Bucket headings.
    expect(await screen.findByText("Your subscriptions")).toBeInTheDocument();
    expect(screen.getByText("Provided by your org")).toBeInTheDocument();

    // Standalone personal subscription is a first-class removable row.
    expect(await screen.findByText("Kimi for Coding")).toBeInTheDocument();

    // The four Xiaomi endpoints collapse to ONE "Xiaomi" card.
    expect(screen.getByText("Xiaomi")).toBeInTheDocument();
    expect(screen.queryByText("Xiaomi Token Plan (China)")).not.toBeInTheDocument();
    expect(screen.queryByText("Xiaomi Token Plan (Europe)")).not.toBeInTheDocument();

    // Z.AI + Zhipu collapse to ONE "Z.AI / Zhipu" card (the separate-rows bug).
    expect(screen.getByText("Z.AI / Zhipu")).toBeInTheDocument();
    expect(screen.queryByText("Zhipu AI")).not.toBeInTheDocument();

    // Multi-plan accounts surface a plan count; each removable card has a Remove.
    expect(screen.getAllByText(/personal subscription · \d+ plans/).length).toBeGreaterThan(0);
    expect(screen.getAllByRole("button", { name: /^remove$/i }).length).toBeGreaterThan(0);

    // Org-provided key shows as managed (no Remove for it).
    expect(await screen.findByText("Anthropic")).toBeInTheDocument();
    expect(screen.getByText("Managed by your org")).toBeInTheDocument();

    // ChatGPT/Codex is its own compact row, not duplicated as a plain provider.
    expect(screen.getByText("ChatGPT / Codex")).toBeInTheDocument();
  });
});
