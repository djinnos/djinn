import { beforeEach, describe, expect, it } from "vitest";
import { ALL_PROJECTS, projectStore } from "./projectStore";
import { mockProjectA, mockProjectB, mockProjects } from "@/test/fixtures";

describe("projectStore", () => {
  beforeEach(() => {
    localStorage.clear();
    projectStore.setState({
      projects: [],
      selectedProjectId: null,
      lastViewPerProject: {},
    });
  });

  it("setProjects sets projects and defaults selectedProjectId to first project", () => {
    projectStore.getState().setProjects(mockProjects);

    expect(projectStore.getState().projects).toEqual(mockProjects);
    expect(projectStore.getState().selectedProjectId).toBe(mockProjectA.id);
  });

  it("setProjects keeps selected project if still present", () => {
    projectStore.getState().setSelectedProjectId(mockProjectB.id);

    projectStore.getState().setProjects(mockProjects);

    expect(projectStore.getState().selectedProjectId).toBe(mockProjectB.id);
  });

  it("setProjects resets selected project if missing", () => {
    projectStore.getState().setSelectedProjectId("missing-project");

    projectStore.getState().setProjects(mockProjects);

    expect(projectStore.getState().selectedProjectId).toBe(mockProjectA.id);
  });

  it("setProjects keeps ALL_PROJECTS selection", () => {
    projectStore.getState().setSelectedProjectId(ALL_PROJECTS);

    projectStore.getState().setProjects(mockProjects);

    expect(projectStore.getState().selectedProjectId).toBe(ALL_PROJECTS);
    expect(projectStore.getState().isAllProjects()).toBe(true);
  });

  it("setSelectedProjectId updates selected project", () => {
    projectStore.getState().setSelectedProjectId(mockProjectB.id);

    expect(projectStore.getState().selectedProjectId).toBe(mockProjectB.id);
  });

  it("getSelectedProject returns the selected project", () => {
    projectStore.getState().setProjects(mockProjects);
    projectStore.getState().setSelectedProjectId(mockProjectB.id);

    expect(projectStore.getState().getSelectedProject()).toEqual(mockProjectB);
  });

  it("getSelectedProject returns undefined for ALL_PROJECTS", () => {
    projectStore.getState().setProjects(mockProjects);
    projectStore.getState().setSelectedProjectId(ALL_PROJECTS);

    expect(projectStore.getState().getSelectedProject()).toBeUndefined();
  });

  it("getLastView defaults to kanban", () => {
    expect(projectStore.getState().getLastView(mockProjectA.id)).toBe("kanban");
  });

  it("setLastView stores and returns project view", () => {
    projectStore.getState().setLastView(mockProjectA.id, "chat");

    expect(projectStore.getState().getLastView(mockProjectA.id)).toBe("chat");
  });

  it("setProjects with empty array clears selectedProjectId", () => {
    projectStore.getState().setSelectedProjectId(mockProjectA.id);

    projectStore.getState().setProjects([]);

    expect(projectStore.getState().selectedProjectId).toBeNull();
  });
});
