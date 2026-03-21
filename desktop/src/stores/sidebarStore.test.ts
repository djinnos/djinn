import { useSidebarStore } from "./sidebarStore";

function resetStore() {
  useSidebarStore.setState({
    activeSection: "kanban",
    projectsExpanded: true,
  });
}

describe("sidebarStore", () => {
  beforeEach(() => {
    resetStore();
  });

  describe("initial state", () => {
    it("starts on kanban section", () => {
      expect(useSidebarStore.getState().activeSection).toBe("kanban");
    });

    it("starts with projects expanded", () => {
      expect(useSidebarStore.getState().projectsExpanded).toBe(true);
    });
  });

  describe("setActiveSection", () => {
    it("sets active section to chat", () => {
      useSidebarStore.getState().setActiveSection("chat");
      expect(useSidebarStore.getState().activeSection).toBe("chat");
    });

    it("sets active section to settings", () => {
      useSidebarStore.getState().setActiveSection("settings");
      expect(useSidebarStore.getState().activeSection).toBe("settings");
    });

    it("sets active section back to kanban", () => {
      useSidebarStore.getState().setActiveSection("settings");
      useSidebarStore.getState().setActiveSection("kanban");
      expect(useSidebarStore.getState().activeSection).toBe("kanban");
    });
  });

  describe("setProjectsExpanded", () => {
    it("collapses projects", () => {
      useSidebarStore.getState().setProjectsExpanded(false);
      expect(useSidebarStore.getState().projectsExpanded).toBe(false);
    });

    it("expands projects", () => {
      useSidebarStore.setState({ projectsExpanded: false });
      useSidebarStore.getState().setProjectsExpanded(true);
      expect(useSidebarStore.getState().projectsExpanded).toBe(true);
    });
  });
});
