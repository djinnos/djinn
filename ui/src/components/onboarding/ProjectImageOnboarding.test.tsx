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

    const pageTitle = await screen.findByRole("heading", {
      level: 1,
      name: "Prepare the runtime environment",
    });
    await waitFor(() => expect(pageTitle).toHaveFocus());
    expect(
      await screen.findByRole("heading", {
        name: "Recommended for this repository",
      }),
    ).toBeInTheDocument();
    expect(await screen.findByText("Node 22 + npm")).toBeInTheDocument();
    expect(
      screen.queryByRole("combobox", { name: "Existing catalog image" }),
    ).not.toBeInTheDocument();

    const create = screen.getByRole("button", {
      name: "Use recommended environment",
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
      await screen.findByRole("heading", {
        level: 1,
        name: "Environment assigned",
      }),
    ).toHaveFocus();
    expect(
      screen.getByText(/finish any required build in the background/i),
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
      await screen.findByRole("button", {
        name: "Use recommended environment",
      }),
    );

    expect(
      await screen.findByRole("heading", { name: "Environment assigned" }),
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
      await screen.findByRole("button", {
        name: "Use recommended environment",
      }),
    );

    expect(
      await screen.findByText(/controller unavailable/i),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("heading", { name: "Environment assigned" }),
    ).not.toBeInTheDocument();
  });

  it("does not allow Advanced image assignment before stack detection", async () => {
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
    await user.click(await screen.findByRole("button", { name: /advanced/i }));
    expect(
      await screen.findByRole("combobox", { name: "Existing catalog image" }),
    ).toBeDisabled();
    expect(screen.getByRole("button", { name: "Use image" })).toBeDisabled();
    expect(
      screen.getByText(/repository detection must finish before an image can be assigned/i),
    ).toBeInTheDocument();
    expect(mocks.setProjectImage).not.toHaveBeenCalled();
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
    await user.click(await screen.findByRole("button", { name: /advanced/i }));
    await user.click(
      await screen.findByRole("combobox", { name: "Existing catalog image" }),
    );
    await user.click(
      await screen.findByRole("option", { name: /Node 22 \+ npm/i }),
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
      await screen.findByRole("button", {
        name: "Use recommended environment",
      }),
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
      await screen.findByRole("button", {
        name: "Use recommended environment",
      }),
    ).toBeDisabled();
    expect(mocks.createImage).not.toHaveBeenCalled();
  });

  it("shows image build status in Advanced and prevents failed-image assignment", async () => {
    const user = userEvent.setup();
    mocks.listImages.mockResolvedValue([
      {
        id: "ready-image",
        name: "Ready image",
        status: "ready",
        config: detectedConfig,
        servicePresets: [],
      },
      {
        id: "building-image",
        name: "Building image",
        status: "building",
        config: detectedConfig,
        servicePresets: [],
      },
      {
        id: "failed-image",
        name: "Failed image",
        status: "failed",
        config: detectedConfig,
        servicePresets: [],
      },
    ]);

    render(<ProjectImageOnboarding project={project} onFinished={vi.fn()} />);

    expect(
      screen.queryByRole("combobox", { name: "Existing catalog image" }),
    ).not.toBeInTheDocument();
    await user.click(await screen.findByRole("button", { name: /advanced/i }));
    await user.click(
      await screen.findByRole("combobox", { name: "Existing catalog image" }),
    );

    const readyStatus = await screen.findByText("Ready");
    const buildingStatus = screen.getByText("Building");
    const failedStatus = screen.getByText("Failed");

    expect(readyStatus.closest('[role="option"]')).not.toHaveAttribute(
      "aria-disabled",
      "true",
    );
    expect(buildingStatus.closest('[role="option"]')).not.toHaveAttribute(
      "aria-disabled",
      "true",
    );
    expect(failedStatus.closest('[role="option"]')).toHaveAttribute(
      "aria-disabled",
      "true",
    );
  });
});
