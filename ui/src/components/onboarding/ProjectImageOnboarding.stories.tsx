/**
 * Onboarding/ProjectImageOnboarding — the required Environment step. A freshly
 * added repository can't dispatch until it references a catalog image, so this
 * gate either creates one from Djinn's detected repository stack or lets the
 * user assign an existing shared image (the "Advanced" accordion).
 *
 * The component fires three `useQuery` calls on mount, all through MCP tools:
 *   - `get_project_stack`      → drives the "detecting…" spinner vs. the
 *     recommended-image card (and the detection-error branch),
 *   - `image_list`             → populates the Advanced existing-image picker,
 *   - `project_environment_config_get` → the project's persisted config.
 * Each story installs a `setMcpToolResponder` in `beforeEach` that varies only
 * the stack response, so the same component renders every phase. `get_project_stack`
 * is polled while `stack` is null, which is exactly the live "detecting" behavior.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { ProjectImageOnboarding } from "./ProjectImageOnboarding";
import { setMcpToolResponder } from "@/storybook-mocks/mcpClient";
import type { Project } from "@/api/server";

// ── Fixtures ────────────────────────────────────────────────────────────────

const project: Project = {
  id: "project-djinn",
  name: "djinnos/djinn",
  github_owner: "djinnos",
  github_repo: "djinn",
};

const emptyConfig = {
  schema_version: 1,
  source: "user-edited",
  languages: {},
  workspaces: [],
  system_packages: [],
  env: {},
  lifecycle: {},
};

const catalogImages = [
  {
    id: "img-rust-node",
    name: "Rust + Node",
    description: "Primary monorepo image.",
    status: "ready",
    config: emptyConfig,
    service_presets: ["postgres-16"],
  },
  {
    id: "img-python",
    name: "Python 3.13",
    description: "Scripting image (mid-build).",
    status: "building",
    config: emptyConfig,
    service_presets: [],
  },
];

const detectedStack = {
  status: "ok",
  stack: {
    primary_language: "rust",
    runtimes: { rust: "1.82", node: "22" },
    package_managers: ["cargo", "pnpm"],
    is_monorepo: true,
    workspaces: [
      { root: ".", language: "rust", toolchain: "1.82" },
      { root: "ui", language: "node", package_manager: "pnpm", toolchain: "22" },
    ],
  },
};

function makeResponder(stackResponse: unknown) {
  return (name: string) => {
    switch (name) {
      case "get_project_stack":
        return stackResponse;
      case "image_list":
        return { status: "ok", images: catalogImages };
      case "project_environment_config_get":
        return {
          status: "ok",
          config: emptyConfig,
          selected_image_id: null,
          selected_image_name: null,
        };
      case "get_project_devcontainer_status":
        return { image_status: "none", graph_warm_status: "pending", needs_image: true };
      default:
        return {};
    }
  };
}

// ── Harness ───────────────────────────────────────────────────────────────

const meta = {
  title: "Onboarding/ProjectImageOnboarding",
  component: ProjectImageOnboarding,
  parameters: { layout: "fullscreen" },
  args: { project, onFinished: () => {} },
  decorators: [
    (Story, ctx) => {
      const queryClient = new QueryClient({
        defaultOptions: { queries: { retry: false, staleTime: Infinity } },
      });
      return (
        <QueryClientProvider client={queryClient}>
          {/* Remount per story so a previous story's cached stack/query
              never bleeds into the next phase. */}
          <Story key={ctx.id} />
        </QueryClientProvider>
      );
    },
  ],
} satisfies Meta<typeof ProjectImageOnboarding>;

export default meta;
type Story = StoryObj<typeof meta>;

/**
 * Repository mirror still cloning — no stack detected yet. The recommended-image
 * card shows the spinner and "Cloning the repository and detecting…" copy while
 * `get_project_stack` polls.
 */
export const Detecting: Story = {
  beforeEach: () => {
    setMcpToolResponder(makeResponder({ status: "ok", stack: null }));
  },
};

/**
 * Detection finished → the recommended environment ("Rust 1.82 · Node 22 · pnpm")
 * is offered, and the Advanced accordion can assign an existing catalog image.
 */
export const RecommendedReady: Story = {
  beforeEach: () => {
    setMcpToolResponder(makeResponder(detectedStack));
  },
};

/**
 * Stack detection returned an error → the inline retry surfaces inside the
 * recommended-image card.
 */
export const DetectionError: Story = {
  beforeEach: () => {
    setMcpToolResponder(
      makeResponder({
        status: "error",
        stack: null,
        error: "Repository mirror failed: could not clone djinnos/djinn.",
      }),
    );
  },
};
