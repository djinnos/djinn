/**
 * Images/ImagesPage — the routed `/images` catalog table.
 *
 * The page imperatively loads two MCP tools on mount (`image_list` +
 * `service_preset_list`, via `listImages` / `listServicePresets`) — there is no
 * TanStack Query seam, so we install a per-tool responder on the aliased
 * `@/api/mcpClient` mock. `listImages` runs each row's `config` through
 * `normalizeConfig`, so the fixtures send raw (server-shaped) config blobs and
 * the languages column ("Rust, Node") is derived exactly as in production.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";
import { MemoryRouter } from "react-router-dom";

import { ImagesPage } from "./ImagesPage";
import { setMcpToolResponder } from "@/storybook-mocks/mcpClient";

// ── Fixtures ─────────────────────────────────────────────────────────────────

// Raw config blobs as the server serializes them; `listImages` normalizes.
function config(languages: Record<string, unknown>): Record<string, unknown> {
  return {
    schema_version: 1,
    source: "user-edited",
    languages,
    workspaces: [],
    system_packages: [],
    env: {},
    lifecycle: {},
  };
}

const servicePresets = [
  {
    id: "postgres-16",
    name: "PostgreSQL 16",
    service_type: "postgres",
    image: "postgres:16",
    conn_env_var: "TEST_POSTGRES_URL",
  },
  {
    id: "redis-7",
    name: "Redis 7",
    service_type: "redis",
    image: "redis:7",
    conn_env_var: "TEST_REDIS_URL",
  },
];

const images = [
  {
    id: "img-rust-node",
    name: "Rust + Node",
    description: "Primary monorepo image — Rust workspace plus the pnpm UI.",
    status: "ready",
    config: config({
      rust: { default_toolchain: "stable" },
      node: { default_version: "22" },
    }),
    service_presets: ["postgres-16"],
  },
  {
    id: "img-python",
    name: "Python 3.13",
    description: "Data + scripting image with Postgres and Redis sidecars.",
    status: "building",
    config: config({ python: { default_version: "3.13" } }),
    service_presets: ["postgres-16", "redis-7"],
  },
  {
    id: "img-go",
    name: "Go 1.26",
    description: "Standalone Go services image.",
    status: "failed",
    config: config({ go: { default_version: "1.26" } }),
    service_presets: [],
  },
];

function populatedResponder(name: string): unknown {
  switch (name) {
    case "image_list":
      return { status: "ok", images };
    case "service_preset_list":
      return { status: "ok", presets: servicePresets };
    default:
      return {};
  }
}

function emptyResponder(name: string): unknown {
  switch (name) {
    case "image_list":
      return { status: "ok", images: [] };
    case "service_preset_list":
      return { status: "ok", presets: servicePresets };
    default:
      return {};
  }
}

// ── Harness ──────────────────────────────────────────────────────────────────

function ImagesStory() {
  return (
    <MemoryRouter initialEntries={["/images"]}>
      <div className="h-screen bg-background text-foreground">
        <ImagesPage />
      </div>
    </MemoryRouter>
  );
}

const meta = {
  title: "Repositories/ImagesPage",
  component: ImagesStory,
  parameters: { layout: "fullscreen" },
} satisfies Meta<typeof ImagesStory>;

export default meta;
type Story = StoryObj<typeof meta>;

/**
 * Three catalog images across the build lifecycle: a ready Rust+Node image with
 * a Postgres sidecar, a building Python image with Postgres+Redis, and a failed
 * Go image with no services.
 */
export const Populated: Story = {
  beforeEach: () => setMcpToolResponder(populatedResponder),
};

/** Empty catalog → the "No images yet" EmptyState with the "New image" CTA. */
export const Empty: Story = {
  beforeEach: () => setMcpToolResponder(emptyResponder),
};
