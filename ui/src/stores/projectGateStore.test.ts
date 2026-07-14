import { beforeEach, describe, expect, it, vi } from "vitest";

import type { Project } from "@/api/server";

const mocks = vi.hoisted(() => ({
  fetchProjects: vi.fn(),
  fetchDevcontainerStatus: vi.fn(),
}));

vi.mock("@/api/server", () => ({
  fetchProjects: mocks.fetchProjects,
}));
vi.mock("@/api/devcontainer", () => ({
  fetchDevcontainerStatus: mocks.fetchDevcontainerStatus,
}));

import { useProjectGateStore } from "./projectGateStore";

const project = {
  id: "project-1",
  name: "Example",
  github_owner: "djinnos",
  github_repo: "example",
} as Project;

const secondProject = {
  id: "project-2",
  name: "Second",
  github_owner: "djinnos",
  github_repo: "second",
} as Project;

describe("projectGateStore", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useProjectGateStore.setState({
      hasProject: null,
      projectNeedingImage: null,
      error: null,
      isChecking: false,
    });
  });

  it("keeps repository onboarding open when no project exists", async () => {
    mocks.fetchProjects.mockResolvedValue([]);

    await useProjectGateStore.getState().refresh();

    expect(useProjectGateStore.getState()).toMatchObject({
      hasProject: false,
      projectNeedingImage: null,
      isChecking: false,
    });
  });

  it("keeps project readiness unresolved when the repository list fails", async () => {
    mocks.fetchProjects.mockRejectedValue(new Error("repository service offline"));

    await useProjectGateStore.getState().refresh();

    expect(useProjectGateStore.getState()).toMatchObject({
      hasProject: null,
      projectNeedingImage: null,
      error: "repository service offline",
      isChecking: false,
    });
  });

  it("uses durable server status to route an image-less project after reload", async () => {
    mocks.fetchProjects.mockResolvedValue([project]);
    mocks.fetchDevcontainerStatus.mockResolvedValue({ needs_image: true });

    await useProjectGateStore.getState().refresh();

    expect(useProjectGateStore.getState()).toMatchObject({
      hasProject: true,
      projectNeedingImage: project,
      isChecking: false,
    });
    expect(mocks.fetchDevcontainerStatus).toHaveBeenCalledWith(project.id);
  });

  it("exposes a just-added project immediately and confirms it from the server", async () => {
    useProjectGateStore.getState().markPendingProject(project);
    expect(useProjectGateStore.getState()).toMatchObject({
      hasProject: true,
      projectNeedingImage: project,
    });

    mocks.fetchProjects.mockResolvedValue([project]);
    mocks.fetchDevcontainerStatus.mockResolvedValue({ needs_image: true });

    await useProjectGateStore.getState().refresh();

    expect(useProjectGateStore.getState()).toMatchObject({
      hasProject: true,
      projectNeedingImage: project,
      isChecking: false,
    });
  });

  it("checks every project and routes the first one whose image is incomplete", async () => {
    mocks.fetchProjects.mockResolvedValue([project, secondProject]);
    mocks.fetchDevcontainerStatus
      .mockResolvedValueOnce({ needs_image: false })
      .mockResolvedValueOnce({ needs_image: true });

    await useProjectGateStore.getState().refresh();

    expect(useProjectGateStore.getState()).toMatchObject({
      hasProject: true,
      projectNeedingImage: secondProject,
      isChecking: false,
    });
    expect(mocks.fetchDevcontainerStatus).toHaveBeenCalledTimes(2);
  });

  it("opens the app only after every project has an assigned image", async () => {
    mocks.fetchProjects.mockResolvedValue([project, secondProject]);
    mocks.fetchDevcontainerStatus.mockResolvedValue({ needs_image: false });

    await useProjectGateStore.getState().refresh();

    expect(useProjectGateStore.getState()).toMatchObject({
      hasProject: true,
      projectNeedingImage: null,
      error: null,
      isChecking: false,
    });
    expect(mocks.fetchDevcontainerStatus).toHaveBeenCalledTimes(2);
  });

  it("does not let a stale completion clear a newer pending project", () => {
    useProjectGateStore.getState().markPendingProject(project);

    useProjectGateStore.getState().clearPendingProject("older-project");
    expect(useProjectGateStore.getState()).toMatchObject({
      projectNeedingImage: project,
    });

    useProjectGateStore.getState().clearPendingProject(project.id);
    expect(useProjectGateStore.getState()).toMatchObject({
      projectNeedingImage: null,
    });
  });

  it("routes the first unresolved status to image setup", async () => {
    mocks.fetchProjects.mockResolvedValue([project, secondProject]);
    mocks.fetchDevcontainerStatus
      .mockRejectedValueOnce(new Error("offline"))
      .mockResolvedValueOnce({ needs_image: true });

    await useProjectGateStore.getState().refresh();

    expect(useProjectGateStore.getState()).toMatchObject({
      hasProject: true,
      projectNeedingImage: project,
      error: "offline",
      isChecking: false,
    });
    expect(mocks.fetchDevcontainerStatus).toHaveBeenCalledTimes(2);
  });

  it("keeps required image setup visible for a resolved semantic status error", async () => {
    mocks.fetchProjects.mockResolvedValue([project]);
    mocks.fetchDevcontainerStatus.mockResolvedValue({
      needs_image: false,
      error: "database unavailable",
    });

    await useProjectGateStore.getState().refresh();

    expect(useProjectGateStore.getState()).toMatchObject({
      hasProject: true,
      projectNeedingImage: project,
      error: "database unavailable",
      isChecking: false,
    });
  });

  it("fails closed when a status payload does not prove image assignment", async () => {
    mocks.fetchProjects.mockResolvedValue([project]);
    mocks.fetchDevcontainerStatus.mockResolvedValue({});

    await useProjectGateStore.getState().refresh();

    expect(useProjectGateStore.getState()).toMatchObject({
      hasProject: true,
      projectNeedingImage: project,
      error: "Image setup status was incomplete",
      isChecking: false,
    });
  });

  it("does not let an older refresh overwrite a newly-marked project", async () => {
    let resolveFirst: ((projects: Project[]) => void) | undefined;
    mocks.fetchProjects
      .mockImplementationOnce(
        () => new Promise<Project[]>((resolve) => {
          resolveFirst = resolve;
        }),
      )
      .mockResolvedValueOnce([project]);
    mocks.fetchDevcontainerStatus.mockResolvedValue({ needs_image: true });

    const staleRefresh = useProjectGateStore.getState().refresh();
    useProjectGateStore.getState().markPendingProject(project);
    const currentRefresh = useProjectGateStore.getState().refresh();

    await currentRefresh;
    resolveFirst?.([]);
    await staleRefresh;

    expect(useProjectGateStore.getState()).toMatchObject({
      hasProject: true,
      projectNeedingImage: project,
      isChecking: false,
    });
  });
});
