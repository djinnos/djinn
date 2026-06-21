import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  DEFAULT_DEPTH,
  EDGE_KINDS,
  LENS_PRESETS,
  MAX_DEPTH,
  MIN_DEPTH,
  NODE_KINDS,
  SYMBOL_KIND_FILTERS,
  useCodeGraphStore,
} from "./codeGraphStore";

describe("codeGraphStore", () => {
  beforeEach(() => {
    window.sessionStorage.clear();
    useCodeGraphStore.getState().reset();
  });

  describe("initial state", () => {
    it("starts with no selection or hover", () => {
      const state = useCodeGraphStore.getState();
      expect(state.selectionId).toBeNull();
      expect(state.hoverId).toBeNull();
    });

    it("starts with empty highlight sets", () => {
      const state = useCodeGraphStore.getState();
      expect(state.citationIds.size).toBe(0);
      expect(state.toolHighlightIds.size).toBe(0);
      expect(state.blastRadiusFrontier.size).toBe(0);
    });

    it("ships only the high-signal semantic spine on by default", () => {
      const filters = useCodeGraphStore.getState().edgeKindFilters;
      const noisy = new Set([
        "ContainsDefinition",
        "DeclaredInFile",
        "FileReference",
        "SymbolReference",
        "Reads",
        "MemberOf",
      ]);
      for (const kind of EDGE_KINDS) {
        expect(filters[kind]).toBe(!noisy.has(kind));
      }
    });

    it("starts with architecture lens defaults (community off, symbol on)", () => {
      const filters = useCodeGraphStore.getState().nodeKindFilters;
      expect(filters.folder).toBe(true);
      expect(filters.file).toBe(true);
      expect(filters.symbol).toBe(true);
      expect(filters.community).toBe(false);
    });

    it("starts with all symbol kinds hidden (architecture lens)", () => {
      const filters = useCodeGraphStore.getState().symbolKindFilters;
      for (const kind of SYMBOL_KIND_FILTERS) {
        expect(filters[kind]).toBe(false);
      }
    });

    it("starts at the default (max) depth", () => {
      expect(useCodeGraphStore.getState().depthFilter).toBe(DEFAULT_DEPTH);
    });

    it("defaults to architecture lens", () => {
      expect(useCodeGraphStore.getState().activeLens).toBe("architecture");
    });

    it("starts with no selected workspace", () => {
      expect(useCodeGraphStore.getState().selectedWorkspaceSlug).toBeNull();
    });
  });

  describe("setSelection", () => {
    it("sets and clears the focal node", () => {
      useCodeGraphStore.getState().setSelection("symbol:foo");
      expect(useCodeGraphStore.getState().selectionId).toBe("symbol:foo");
      useCodeGraphStore.getState().setSelection(null);
      expect(useCodeGraphStore.getState().selectionId).toBeNull();
    });
  });

  describe("citations", () => {
    it("setCitations replaces the set with new ids", () => {
      useCodeGraphStore.getState().setCitations(["a", "b"]);
      const ids = useCodeGraphStore.getState().citationIds;
      expect(ids.has("a")).toBe(true);
      expect(ids.has("b")).toBe(true);
      expect(ids.size).toBe(2);
    });

    it("setCitations replaces (does not merge) on subsequent calls", () => {
      useCodeGraphStore.getState().setCitations(["a"]);
      useCodeGraphStore.getState().setCitations(["b"]);
      const ids = useCodeGraphStore.getState().citationIds;
      expect(ids.has("a")).toBe(false);
      expect(ids.has("b")).toBe(true);
    });

    it("clearCitations empties the set", () => {
      useCodeGraphStore.getState().setCitations(["a", "b"]);
      useCodeGraphStore.getState().clearCitations();
      expect(useCodeGraphStore.getState().citationIds.size).toBe(0);
    });

    it("setCitations accepts iterables (e.g. Set)", () => {
      useCodeGraphStore.getState().setCitations(new Set(["x", "y", "x"]));
      expect(useCodeGraphStore.getState().citationIds.size).toBe(2);
    });
  });

  describe("toolHighlight + blastRadius", () => {
    it("setToolHighlight populates the set", () => {
      useCodeGraphStore.getState().setToolHighlight(["a", "b", "c"]);
      expect(useCodeGraphStore.getState().toolHighlightIds.size).toBe(3);
    });

    it("clearToolHighlight empties the set", () => {
      useCodeGraphStore.getState().setToolHighlight(["a"]);
      useCodeGraphStore.getState().clearToolHighlight();
      expect(useCodeGraphStore.getState().toolHighlightIds.size).toBe(0);
    });

    it("blastRadiusFrontier is independent of toolHighlight", () => {
      useCodeGraphStore.getState().setToolHighlight(["a"]);
      useCodeGraphStore.getState().setBlastRadiusFrontier(["b"]);
      expect(useCodeGraphStore.getState().toolHighlightIds.has("a")).toBe(true);
      expect(useCodeGraphStore.getState().blastRadiusFrontier.has("b")).toBe(true);
    });
  });

  describe("hover", () => {
    it("setHover stores and clears", () => {
      useCodeGraphStore.getState().setHover("foo");
      expect(useCodeGraphStore.getState().hoverId).toBe("foo");
      useCodeGraphStore.getState().setHover(null);
      expect(useCodeGraphStore.getState().hoverId).toBeNull();
    });
  });

  describe("edgeKindFilters", () => {
    it("toggleEdgeKind flips a kind on/off", () => {
      useCodeGraphStore.getState().toggleEdgeKind("Implements");
      expect(useCodeGraphStore.getState().edgeKindFilters.Implements).toBe(false);
      useCodeGraphStore.getState().toggleEdgeKind("Implements");
      expect(useCodeGraphStore.getState().edgeKindFilters.Implements).toBe(true);
    });

    it("toggleEdgeKind treats missing keys as enabled", () => {
      // An unknown kind starts implicitly true; the first toggle flips it false.
      useCodeGraphStore.getState().toggleEdgeKind("MadeUpKind");
      expect(useCodeGraphStore.getState().edgeKindFilters.MadeUpKind).toBe(false);
    });

    it("setEdgeKindEnabled writes the explicit value", () => {
      useCodeGraphStore.getState().setEdgeKindEnabled("Writes", false);
      expect(useCodeGraphStore.getState().edgeKindFilters.Writes).toBe(false);
      useCodeGraphStore.getState().setEdgeKindEnabled("Writes", true);
      expect(useCodeGraphStore.getState().edgeKindFilters.Writes).toBe(true);
    });
  });

  describe("setDepthFilter", () => {
    it("clamps below MIN_DEPTH", () => {
      useCodeGraphStore.getState().setDepthFilter(0);
      expect(useCodeGraphStore.getState().depthFilter).toBe(MIN_DEPTH);
      useCodeGraphStore.getState().setDepthFilter(-3);
      expect(useCodeGraphStore.getState().depthFilter).toBe(MIN_DEPTH);
    });

    it("clamps above MAX_DEPTH", () => {
      useCodeGraphStore.getState().setDepthFilter(99);
      expect(useCodeGraphStore.getState().depthFilter).toBe(MAX_DEPTH);
    });

    it("rounds non-integer input", () => {
      useCodeGraphStore.getState().setDepthFilter(2.6);
      expect(useCodeGraphStore.getState().depthFilter).toBe(3);
    });
  });

  describe("colorMode (iter 30)", () => {
    it("defaults to topology", () => {
      expect(useCodeGraphStore.getState().colorMode).toBe("topology");
    });

    it("setColorMode flips between topology and complexity", () => {
      useCodeGraphStore.getState().setColorMode("complexity");
      expect(useCodeGraphStore.getState().colorMode).toBe("complexity");
      useCodeGraphStore.getState().setColorMode("topology");
      expect(useCodeGraphStore.getState().colorMode).toBe("topology");
    });

    it("setComplexityAvailable(false) snaps mode back to topology to avoid degenerate gradient", () => {
      const s = useCodeGraphStore.getState();
      s.setComplexityAvailable(true);
      s.setColorMode("complexity");
      expect(useCodeGraphStore.getState().colorMode).toBe("complexity");
      s.setComplexityAvailable(false);
      const after = useCodeGraphStore.getState();
      expect(after.complexityAvailable).toBe(false);
      expect(after.colorMode).toBe("topology");
    });

    it("setComplexityAvailable(false) leaves mode alone when already on topology", () => {
      const s = useCodeGraphStore.getState();
      s.setComplexityAvailable(true);
      s.setComplexityAvailable(false);
      expect(useCodeGraphStore.getState().colorMode).toBe("topology");
    });
  });

  describe("selectedWorkspaceSlug", () => {
    it("sets and clears the selected workspace slug", () => {
      useCodeGraphStore.getState().setSelectedWorkspaceSlug("api");
      expect(useCodeGraphStore.getState().selectedWorkspaceSlug).toBe("api");
      useCodeGraphStore.getState().setSelectedWorkspaceSlug(null);
      expect(useCodeGraphStore.getState().selectedWorkspaceSlug).toBeNull();
    });
  });

  describe("semanticZoomMode", () => {
    it("defaults to auto", () => {
      expect(useCodeGraphStore.getState().semanticZoomMode).toBe("auto");
    });

    it("setSemanticZoomMode updates the mode", () => {
      useCodeGraphStore.getState().setSemanticZoomMode("symbol");
      expect(useCodeGraphStore.getState().semanticZoomMode).toBe("symbol");
      useCodeGraphStore.getState().setSemanticZoomMode("community");
      expect(useCodeGraphStore.getState().semanticZoomMode).toBe("community");
      useCodeGraphStore.getState().setSemanticZoomMode("auto");
      expect(useCodeGraphStore.getState().semanticZoomMode).toBe("auto");
    });
  });

  describe("expandedCommunityIds", () => {
    it("defaults to an empty set", () => {
      expect(useCodeGraphStore.getState().expandedCommunityIds.size).toBe(0);
    });

    it("expandCommunity adds a stable community_id", () => {
      useCodeGraphStore.getState().expandCommunity("auth");
      expect(
        useCodeGraphStore.getState().expandedCommunityIds.has("auth"),
      ).toBe(true);
    });

    it("expandCommunity is idempotent", () => {
      useCodeGraphStore.getState().expandCommunity("auth");
      useCodeGraphStore.getState().expandCommunity("auth");
      expect(useCodeGraphStore.getState().expandedCommunityIds.size).toBe(1);
    });

    it("collapseCommunity removes a community_id", () => {
      useCodeGraphStore.getState().expandCommunity("auth");
      useCodeGraphStore.getState().collapseCommunity("auth");
      expect(
        useCodeGraphStore.getState().expandedCommunityIds.has("auth"),
      ).toBe(false);
    });

    it("collapseCommunity is idempotent when not expanded", () => {
      useCodeGraphStore.getState().collapseCommunity("auth");
      expect(
        useCodeGraphStore.getState().expandedCommunityIds.has("auth"),
      ).toBe(false);
    });

    it("clearExpandedCommunities empties the set", () => {
      useCodeGraphStore.getState().expandCommunity("auth");
      useCodeGraphStore.getState().expandCommunity("api");
      useCodeGraphStore.getState().clearExpandedCommunities();
      expect(useCodeGraphStore.getState().expandedCommunityIds.size).toBe(0);
    });
  });

  describe("intent lenses", () => {
    it("defaults to architecture lens", () => {
      expect(useCodeGraphStore.getState().activeLens).toBe("architecture");
    });

    it("applyLens sets all three filter records from the preset", () => {
      useCodeGraphStore.getState().applyLens("calls");
      const state = useCodeGraphStore.getState();
      expect(state.nodeKindFilters).toEqual(LENS_PRESETS.calls.nodeKindFilters);
      expect(state.symbolKindFilters).toEqual(
        LENS_PRESETS.calls.symbolKindFilters,
      );
      expect(state.edgeKindFilters).toEqual(
        LENS_PRESETS.calls.edgeKindFilters,
      );
    });

    it("applyLens updates activeLens", () => {
      useCodeGraphStore.getState().applyLens("types");
      expect(useCodeGraphStore.getState().activeLens).toBe("types");
    });

    it("toggleEdgeKind sets activeLens to null", () => {
      useCodeGraphStore.getState().applyLens("calls");
      expect(useCodeGraphStore.getState().activeLens).toBe("calls");
      useCodeGraphStore.getState().toggleEdgeKind("Defines");
      expect(useCodeGraphStore.getState().activeLens).toBeNull();
    });

    it("toggleNodeKind sets activeLens to null", () => {
      useCodeGraphStore.getState().applyLens("types");
      expect(useCodeGraphStore.getState().activeLens).toBe("types");
      useCodeGraphStore.getState().toggleNodeKind("symbol");
      expect(useCodeGraphStore.getState().activeLens).toBeNull();
    });

    it("toggleSymbolKind sets activeLens to null", () => {
      useCodeGraphStore.getState().applyLens("dataflow");
      expect(useCodeGraphStore.getState().activeLens).toBe("dataflow");
      useCodeGraphStore.getState().toggleSymbolKind("function");
      expect(useCodeGraphStore.getState().activeLens).toBeNull();
    });

    it("each lens preset covers all NODE_KINDS keys", () => {
      for (const preset of Object.values(LENS_PRESETS)) {
        for (const kind of NODE_KINDS) {
          expect(preset.nodeKindFilters).toHaveProperty(kind);
        }
      }
    });

    it("each lens preset covers all SYMBOL_KIND_FILTERS keys", () => {
      for (const preset of Object.values(LENS_PRESETS)) {
        for (const kind of SYMBOL_KIND_FILTERS) {
          expect(preset.symbolKindFilters).toHaveProperty(kind);
        }
      }
    });

    it("each lens preset covers all EDGE_KINDS keys", () => {
      for (const preset of Object.values(LENS_PRESETS)) {
        for (const kind of EDGE_KINDS) {
          expect(preset.edgeKindFilters).toHaveProperty(kind);
        }
      }
    });
  });

  describe("reset", () => {
    it("returns every slice to its default", () => {
      const s = useCodeGraphStore.getState();
      s.setSelectedWorkspaceSlug("api");
      s.setSelection("foo");
      s.setCitations(["a"]);
      s.setToolHighlight(["b"]);
      s.setBlastRadiusFrontier(["c"]);
      s.setHover("foo");
      s.toggleEdgeKind("Implements");
      s.setDepthFilter(1);
      s.expandCommunity("auth");

      useCodeGraphStore.getState().reset();
      const after = useCodeGraphStore.getState();
      expect(after.selectionId).toBeNull();
      expect(after.hoverId).toBeNull();
      expect(after.citationIds.size).toBe(0);
      expect(after.toolHighlightIds.size).toBe(0);
      expect(after.blastRadiusFrontier.size).toBe(0);
      expect(after.depthFilter).toBe(DEFAULT_DEPTH);
      expect(after.edgeKindFilters.Implements).toBe(true);
      expect(after.edgeKindFilters.Reads).toBe(false);
      expect(after.edgeKindFilters.FileReference).toBe(false);
      expect(after.activeLens).toBe("architecture");
      expect(after.colorMode).toBe("topology");
      expect(after.complexityAvailable).toBe(false);
      expect(after.semanticZoomMode).toBe("auto");
      expect(after.expandedCommunityIds.size).toBe(0);
      expect(after.selectedWorkspaceSlug).toBe("api");
    });
  });
});
