/**
 * Images/ImageEditorPage — the full-page create/edit flow for a catalog image.
 *
 * Routes: `/images/new` (create) and `/images/:id/edit` (edit). The page and
 * its embedded `ImageConfigEditor` pull three MCP tools:
 *   - `service_preset_list` (the injected-services picker),
 *   - `toolchain_versions` (the per-language version selectors — TanStack Query
 *     inside `ImageConfigEditor`; served here through the same responder), and
 *   - `image_list` (edit route only, to seed the form from the target image).
 * A per-tool responder on the aliased `@/api/mcpClient` mock serves all three;
 * a per-story `QueryClient` (retry:false) wraps the page so the toolchain query
 * resolves from the fixture instead of a live backend.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { ImageEditorPage } from "./ImageEditorPage";
import { setMcpToolResponder } from "@/storybook-mocks/mcpClient";

// ── Fixtures ─────────────────────────────────────────────────────────────────

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
  {
    id: "rabbitmq-3",
    name: "RabbitMQ 3",
    service_type: "rabbitmq",
    image: "rabbitmq:3",
    conn_env_var: "TEST_AMQP_URL",
  },
];

const toolchainVersions = {
  rust: ["stable", "beta", "nightly"],
  node: ["lts", "24", "22", "20"],
  python: ["3.13", "3.12", "3.11"],
  go: ["1.26", "1.25", "1.24"],
};

// The image the edit route seeds its form from (matched by id from the URL).
const editImage = {
  id: "img-rust-node",
  name: "Rust + Node",
  description: "Primary monorepo image — Rust workspace plus the pnpm UI.",
  status: "ready",
  config: {
    schema_version: 1,
    source: "user-edited",
    languages: {
      rust: { default_toolchain: "stable" },
      node: { default_version: "22" },
    },
    workspaces: [],
    system_packages: ["postgresql-client", "protobuf-compiler"],
    env: { RUST_BACKTRACE: "1" },
    lifecycle: {},
  },
  service_presets: ["postgres-16"],
};

function responder(name: string): unknown {
  switch (name) {
    case "service_preset_list":
      return { status: "ok", presets: servicePresets };
    case "toolchain_versions":
      return { status: "ok", versions: toolchainVersions };
    case "image_list":
      return { status: "ok", images: [editImage] };
    default:
      return {};
  }
}

// ── Harness ──────────────────────────────────────────────────────────────────

interface EditorStoryArgs {
  initialPath: string;
}

function ImageEditorStory({ initialPath }: EditorStoryArgs) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  return (
    <QueryClientProvider client={queryClient}>
      <MemoryRouter initialEntries={[initialPath]}>
        <div className="h-screen bg-background text-foreground">
          <Routes>
            <Route path="/images/new" element={<ImageEditorPage />} />
            <Route path="/images/:id/edit" element={<ImageEditorPage />} />
            <Route path="/images" element={<div />} />
          </Routes>
        </div>
      </MemoryRouter>
    </QueryClientProvider>
  );
}

const meta = {
  title: "Images/ImageEditorPage",
  component: ImageEditorStory,
  parameters: { layout: "fullscreen" },
  beforeEach: () => setMcpToolResponder(responder),
} satisfies Meta<typeof ImageEditorStory>;

export default meta;
type Story = StoryObj<typeof meta>;

/**
 * Create route (`/images/new`): empty name/description, the Form tab with every
 * language toggle off, and the full injected-services picker.
 */
export const Create: Story = {
  args: { initialPath: "/images/new" },
};

/**
 * Edit route (`/images/:id/edit`): the form is seeded from the "Rust + Node"
 * catalog image — name/description filled, Rust + Node enabled with their
 * versions, two system packages, an env var, and the Postgres service toggled
 * on.
 */
export const Edit: Story = {
  args: { initialPath: "/images/img-rust-node/edit" },
};
