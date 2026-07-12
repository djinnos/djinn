import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor, render, userEvent } from "@/test/test-utils";

import type { Project } from "@/api/server";
import { normalizeConfig } from "@/api/environmentConfig";

const mocks = vi.hoisted(() => ({
  createImage: vi.fn(),
  listImages: vi.fn(),
  setProjectImage: vi.fn(),
  fetchDevcontainerStatus: vi.fn(),
  fetchProjectStack: vi.fn(),
  fetchEnvironmentConfig: vi.fn(),
  resetEnvironmentConfig: vi.fn(),
}));

vi.mock("@/api/images", () => ({
  createImage: mocks.createImage,
  listImages: mocks.listImages,
  setProjectImage: mocks.setProjectImage,
}));
vi.mock("@/api/devcontainer", () => ({
  fetchDevcontainerStatus: mocks.fetchDevcontainerStatus,
  fetchProjectStack: mocks.fetchProjectStack,
}));
vi.mock("@/api/environmentConfig", async () => {
  const actual = await vi.importActual<typeof import("@/api/environmentConfig")>(
    "@/api/environmentConfig",
  );
  return {
    ...actual,
    fetchEnvironmentConfig: mocks.fetchEnvironmentConfig,
    resetEnvironmentConfig: mocks.resetEnvironmentConfig,
  };
});

import { ProjectImageOnboarding } from "./ProjectImageOnboarding";

const project = {
  id: "project-1",
  name: "Example",
  github_owner: "djinnos",
  github_repo: "example",
} as Project;

const detectedConfig = normalizeConfig({
  schema_version: 1,
  languages: {
    node: { default_version: "22", default_package_manager: "npm" },
  },
  workspaces: [
    { root: ".", language: "node", version: "22", package_manager: "npm" },
  ],
});

describe("ProjectImageOnboarding", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.listImages.mockResolvedValue([]);
    mocks.fetchProjectStack.mockResolvedValue({
      stack: {
        detected_at: "2026-07-12T00:00:00Z",
        languages: [{ name: "node", bytes: 100, pct: 100 }],
        runtimes: { node: "22" },
        package_managers: ["npm"],
        workspaces: [
          { root: ".", language: "node", toolchain: "22", package_manager: "npm" },
        ],
      },
    });
    mocks.fetchEnvironmentConfig.mockResolvedValue({
      config: normalizeConfig({ schema_version: 0 }),
      seeded: false,
      selectedImageId: null,
      selectedImageName: null,
    });
    mocks.resetEnvironmentConfig.mockResolvedValue({
      ok: true,
      config: detectedConfig,
    });
    mocks.createImage.mockResolvedValue({ ok: true, id: "image-1" });
    mocks.setProjectImage.mockResolvedValue({ ok: true });
    mocks.fetchDevcontainerStatus.mockResolvedValue({ needs_image: false });
  });

  it("creates a reusable detected image and assigns it before entering Djinn", async () => {
    const user = userEvent.setup();
    const onFinished = vi.fn();
    render(<ProjectImageOnboarding project={project} onFinished={onFinished} />);

    const create = await screen.findByRole("button", {
      name: "Create detected image",
    });
    await user.click(create);

    await waitFor(() => expect(mocks.createImage).toHaveBeenCalledOnce());
    expect(mocks.resetEnvironmentConfig).toHaveBeenCalledWith("project-1");
    expect(mocks.createImage).toHaveBeenCalledWith(
      expect.objectContaining({
        name: "Node 22 + npm",
        config: {
          schema_version: 1,
          source: "auto-detected",
          languages: {
            node: { default_version: "22", default_package_manager: "npm" },
          },
          workspaces: [],
          system_packages: [],
          env: {},
          lifecycle: { post_build: [], pre_anything: [], pre_task: [] },
        },
      }),
    );
    expect(mocks.setProjectImage).toHaveBeenCalledWith("project-1", "image-1");
    expect(
      await screen.findByRole("heading", { name: "Environment build started" }),
    ).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: /enter djinn/i }));
    expect(onFinished).toHaveBeenCalledOnce();
  });

  it("recovers when assignment persisted before build enqueue reported an error", async () => {
    const user = userEvent.setup();
    mocks.setProjectImage.mockResolvedValue({
      ok: false,
      error: "enqueue image build: controller unavailable",
    });
    mocks.fetchDevcontainerStatus.mockResolvedValue({ needs_image: false });

    render(<ProjectImageOnboarding project={project} onFinished={vi.fn()} />);
    await user.click(
      await screen.findByRole("button", { name: "Create detected image" }),
    );

    expect(
      await screen.findByRole("heading", { name: "Environment build started" }),
    ).toBeInTheDocument();
    expect(screen.queryByText(/controller unavailable/i)).not.toBeInTheDocument();
  });

  it("does not mistake a semantic status error for a persisted assignment", async () => {
    const user = userEvent.setup();
    mocks.setProjectImage.mockResolvedValue({
      ok: false,
      error: "enqueue image build: controller unavailable",
    });
    mocks.fetchDevcontainerStatus.mockResolvedValue({
      needs_image: false,
      error: "database unavailable",
    });

    render(<ProjectImageOnboarding project={project} onFinished={vi.fn()} />);
    await user.click(
      await screen.findByRole("button", { name: "Create detected image" }),
    );

    expect(
      await screen.findByText(/controller unavailable/i),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("heading", { name: "Environment build started" }),
    ).not.toBeInTheDocument();
  });

  it("assigns an existing catalog image without waiting for stack detection", async () => {
    const user = userEvent.setup();
    mocks.fetchProjectStack.mockResolvedValue({ stack: null });
    mocks.listImages.mockResolvedValue([
      {
        id: "shared-node",
        name: "Node 22 + npm",
        status: "ready",
        config: detectedConfig,
        servicePresets: [],
      },
    ]);

    render(<ProjectImageOnboarding project={project} onFinished={vi.fn()} />);
    await user.click(await screen.findByRole("combobox"));
    await user.click(
      await screen.findByRole("option", { name: "Node 22 + npm" }),
    );
    await user.click(screen.getByRole("button", { name: "Use image" }));

    await waitFor(() =>
      expect(mocks.setProjectImage).toHaveBeenCalledWith("project-1", "shared-node"),
    );
    expect(mocks.resetEnvironmentConfig).not.toHaveBeenCalled();
    expect(mocks.createImage).not.toHaveBeenCalled();
  });

  it("persists detected project workspaces before assigning an existing image", async () => {
    const user = userEvent.setup();
    mocks.listImages.mockResolvedValue([
      {
        id: "shared-node",
        name: "Node 22 + npm",
        status: "ready",
        config: detectedConfig,
        servicePresets: [],
      },
    ]);

    render(<ProjectImageOnboarding project={project} onFinished={vi.fn()} />);
    await user.click(await screen.findByRole("combobox"));
    await user.click(
      await screen.findByRole("option", { name: "Node 22 + npm" }),
    );
    await user.click(screen.getByRole("button", { name: "Use image" }));

    await waitFor(() =>
      expect(mocks.setProjectImage).toHaveBeenCalledWith("project-1", "shared-node"),
    );
    expect(mocks.resetEnvironmentConfig).toHaveBeenCalledWith("project-1");
    expect(mocks.resetEnvironmentConfig.mock.invocationCallOrder[0]).toBeLessThan(
      mocks.setProjectImage.mock.invocationCallOrder[0],
    );
    expect(mocks.createImage).not.toHaveBeenCalled();
  });

  it("never copies a seeded project environment into the shared image", async () => {
    const user = userEvent.setup();
    const userEdited = {
      ...detectedConfig,
      source: "user-edited" as const,
      system_packages: ["libvips-dev"],
      env: { PRIVATE_TOKEN: "must-not-leak" },
      lifecycle: {
        ...detectedConfig.lifecycle,
        post_build: ["./project-only-setup.sh"],
      },
    };
    mocks.fetchEnvironmentConfig.mockResolvedValue({
      config: userEdited,
      seeded: true,
      selectedImageId: null,
      selectedImageName: null,
    });

    render(<ProjectImageOnboarding project={project} onFinished={vi.fn()} />);
    await user.click(
      await screen.findByRole("button", { name: "Create detected image" }),
    );

    await waitFor(() => expect(mocks.createImage).toHaveBeenCalledOnce());
    expect(mocks.resetEnvironmentConfig).not.toHaveBeenCalled();
    expect(mocks.createImage).toHaveBeenCalledWith(
      expect.objectContaining({
        config: {
          schema_version: 1,
          source: "auto-detected",
          languages: {
            node: { default_version: "22", default_package_manager: "npm" },
          },
          workspaces: [],
          system_packages: [],
          env: {},
          lifecycle: { post_build: [], pre_anything: [], pre_task: [] },
        },
      }),
    );
  });

  it("does not auto-create from a seeded project config until stack detection completes", async () => {
    mocks.fetchProjectStack.mockResolvedValue({ stack: null });
    mocks.fetchEnvironmentConfig.mockResolvedValue({
      config: detectedConfig,
      seeded: true,
      selectedImageId: null,
      selectedImageName: null,
    });

    render(<ProjectImageOnboarding project={project} onFinished={vi.fn()} />);

    expect(
      await screen.findByRole("button", { name: "Create detected image" }),
    ).toBeDisabled();
    expect(mocks.createImage).not.toHaveBeenCalled();
  });
});
