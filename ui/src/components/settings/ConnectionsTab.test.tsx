import { describe, expect, it, vi } from "vitest";

import type { ConnectedProvider } from "@/api/userConfig";
import { render, screen } from "@/test/test-utils";

import { ConnectionsTab } from "./ConnectionsTab";

vi.mock("@/lib/toast", () => ({
  showToast: { success: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

const connected: ConnectedProvider[] = [
  // Codex sub (oauth on openai) — rendered by its own card, not a row.
  {
    id: "openai",
    name: "OpenAI",
    connection_methods: ["oauth"],
    env_vars: ["OPENAI_API_KEY"],
  } as ConnectedProvider,
  // A personal subscription connected by API key.
  {
    id: "kimi-for-coding",
    name: "Kimi for Coding",
    connection_methods: ["credential"],
    env_vars: ["KIMI_API_KEY"],
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
  it("splits connected providers into subscription vs org-provided buckets", async () => {
    render(<ConnectionsTab />);

    // Bucket headings.
    expect(await screen.findByText("Your subscriptions")).toBeInTheDocument();
    expect(screen.getByText("Provided by your org")).toBeInTheDocument();

    // Personal subscription is a first-class removable row.
    expect(await screen.findByText("Kimi for Coding")).toBeInTheDocument();
    expect(screen.getByText("Connected · personal subscription")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /^remove$/i })).toBeInTheDocument();

    // Org-provided key shows as managed (no remove button for it).
    expect(await screen.findByText("Anthropic")).toBeInTheDocument();
    expect(screen.getByText("Managed by your org")).toBeInTheDocument();

    // The Codex/openai oauth row is NOT duplicated as a plain provider row —
    // it's owned by the ChatGPT/Codex card. The card heading is present.
    expect(screen.getByText("ChatGPT / Codex")).toBeInTheDocument();
  });
});
