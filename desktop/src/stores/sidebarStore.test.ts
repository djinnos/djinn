import { useSidebarStore } from "./sidebarStore";

function resetStore() {
  useSidebarStore.setState({
    isCollapsed: false,
    activeSection: "kanban",
    projectsExpanded: true,
  });
}

describe("sidebarStore", () => {
  beforeEach(() => {
    resetStore();
  });

  describe("initial state", () => {
    it("starts expanded", () => {
      expect(useSidebarStore.getState().isCollapsed).toBe(false);
    });

    it("starts on kanban section", () => {
      expect(useSidebarStore.getState().activeSection).toBe("kanban");
    });

    it("starts with projects expanded", () => {
      expect(useSidebarStore.getState().projectsExpanded).toBe(true);
    });
  });

  describe("toggleCollapse", () => {
    it("collapses when expanded", () => {
      useSidebarStore.getState().toggleCollapse();
      expect(useSidebarStore.getState().isCollapsed).toBe(true);
    });

    it("expands when collapsed", () => {
      useSidebarStore.setState({ isCollapsed: true });
      useSidebarStore.getState().toggleCollapse();
      expect(useSidebarStore.getState().isCollapsed).toBe(false);
    });

    it("toggles back and forth", () => {
      useSidebarStore.getState().toggleCollapse();
      expect(useSidebarStore.getState().isCollapsed).toBe(true);
      useSidebarStore.getState().toggleCollapse();
      expect(useSidebarStore.getState().isCollapsed).toBe(false);
    });
  });

  describe("setCollapsed", () => {
    it("sets collapsed to true", () => {
      useSidebarStore.getState().setCollapsed(true);
      expect(useSidebarStore.getState().isCollapsed).toBe(true);
    });

    it("sets collapsed to false", () => {
      useSidebarStore.setState({ isCollapsed: true });
      useSidebarStore.getState().setCollapsed(false);
      expect(useSidebarStore.getState().isCollapsed).toBe(false);
    });
  });

  describe("setActiveSection", () => {
    it("sets active section to epics", () => {
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

  describe("independence", () => {
    it("changing collapse does not affect activeSection", () => {
      useSidebarStore.getState().setActiveSection("chat");
      useSidebarStore.getState().toggleCollapse();
      expect(useSidebarStore.getState().activeSection).toBe("chat");
    });

    it("changing activeSection does not affect collapse", () => {
      useSidebarStore.getState().toggleCollapse();
      useSidebarStore.getState().setActiveSection("settings");
      expect(useSidebarStore.getState().isCollapsed).toBe(true);
    });

    it("changing projects expanded does not affect other state", () => {
      useSidebarStore.getState().setActiveSection("chat");
      useSidebarStore.getState().toggleCollapse();
      useSidebarStore.getState().setProjectsExpanded(false);

      expect(useSidebarStore.getState().activeSection).toBe("chat");
      expect(useSidebarStore.getState().isCollapsed).toBe(true);
      expect(useSidebarStore.getState().projectsExpanded).toBe(false);
    });
  });
});
