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

describe("projectGateStore", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    window.localStorage.clear();
    useProjectGateStore.setState({
      hasProject: null,
      projectNeedingImage: null,
      pendingProjectId: null,
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

  it("does not turn legacy image-less projects into a global onboarding gate", async () => {
    mocks.fetchProjects.mockResolvedValue([project]);

    await useProjectGateStore.getState().refresh();

    expect(useProjectGateStore.getState()).toMatchObject({
      hasProject: true,
      projectNeedingImage: null,
      pendingProjectId: null,
      isChecking: false,
    });
    expect(mocks.fetchDevcontainerStatus).not.toHaveBeenCalled();
  });

  it("persists and routes only the project just added by this browser", async () => {
    useProjectGateStore.getState().markPendingProject(project);
    mocks.fetchProjects.mockResolvedValue([project]);
    mocks.fetchDevcontainerStatus.mockResolvedValue({ needs_image: true });

    await useProjectGateStore.getState().refresh();

    expect(useProjectGateStore.getState()).toMatchObject({
      hasProject: true,
      projectNeedingImage: project,
      pendingProjectId: project.id,
      isChecking: false,
    });
    expect(window.localStorage.length).toBe(1);
    expect(window.localStorage.getItem(window.localStorage.key(0)!)).toBe(project.id);
  });

  it("clears pending setup after the image has been assigned", async () => {
    useProjectGateStore.getState().markPendingProject(project);
    mocks.fetchProjects.mockResolvedValue([project]);
    mocks.fetchDevcontainerStatus.mockResolvedValue({ needs_image: false });

    await useProjectGateStore.getState().refresh();

    expect(useProjectGateStore.getState()).toMatchObject({
      hasProject: true,
      projectNeedingImage: null,
      pendingProjectId: null,
      isChecking: false,
    });
    expect(window.localStorage.length).toBe(0);
  });

  it("does not let a stale completion clear a newer pending project", () => {
    useProjectGateStore.getState().markPendingProject(project);

    useProjectGateStore.getState().clearPendingProject("older-project");
    expect(useProjectGateStore.getState()).toMatchObject({
      projectNeedingImage: project,
      pendingProjectId: project.id,
    });

    useProjectGateStore.getState().clearPendingProject(project.id);
    expect(useProjectGateStore.getState()).toMatchObject({
      projectNeedingImage: null,
      pendingProjectId: null,
    });
    expect(window.localStorage.length).toBe(0);
  });

  it("opens the app on a transient status error but retains reload recovery", async () => {
    useProjectGateStore.getState().markPendingProject(project);
    mocks.fetchProjects.mockResolvedValue([project]);
    mocks.fetchDevcontainerStatus.mockRejectedValue(new Error("offline"));

    await useProjectGateStore.getState().refresh();

    expect(useProjectGateStore.getState()).toMatchObject({
      hasProject: true,
      projectNeedingImage: null,
      pendingProjectId: project.id,
      isChecking: false,
    });
    expect(window.localStorage.length).toBe(1);
  });

  it("retains reload recovery for a resolved semantic status error", async () => {
    useProjectGateStore.getState().markPendingProject(project);
    mocks.fetchProjects.mockResolvedValue([project]);
    mocks.fetchDevcontainerStatus.mockResolvedValue({
      needs_image: false,
      error: "database unavailable",
    });

    await useProjectGateStore.getState().refresh();

    expect(useProjectGateStore.getState()).toMatchObject({
      hasProject: true,
      projectNeedingImage: null,
      pendingProjectId: project.id,
      isChecking: false,
    });
    expect(window.localStorage.length).toBe(1);
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
      pendingProjectId: project.id,
      isChecking: false,
    });
  });
});
