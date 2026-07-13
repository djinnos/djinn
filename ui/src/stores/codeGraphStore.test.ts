import { beforeEach, describe, expect, it } from "vitest";

import {
  CONTAINMENT_EDGE_KINDS,
  DEFAULT_DOI_REVEAL_COUNT,
  DEFAULT_FOCUS_DIRECTION,
  EDGE_KINDS,
  LENS_PRESETS,
  MAX_DOI_REVEAL_COUNT,
  MIN_DOI_REVEAL_COUNT,
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

    it("starts with architecture lens defaults (folder/file/symbol on)", () => {
      const filters = useCodeGraphStore.getState().nodeKindFilters;
      expect(filters.folder).toBe(true);
      expect(filters.file).toBe(true);
      expect(filters.symbol).toBe(true);
      expect(filters.community).toBeUndefined();
    });

    it("starts with all symbol kinds hidden (architecture lens)", () => {
      const filters = useCodeGraphStore.getState().symbolKindFilters;
      for (const kind of SYMBOL_KIND_FILTERS) {
        expect(filters[kind]).toBe(false);
      }
    });

    it("starts with default DOI focus state", () => {
      const state = useCodeGraphStore.getState();
      expect(state.focusAnchorId).toBeNull();
      expect(state.focusDirection).toBe(DEFAULT_FOCUS_DIRECTION);
      expect(state.doiRevealCount).toBe(DEFAULT_DOI_REVEAL_COUNT);
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

    it("toggleEdgeKind is a no-op for containment edge kinds", () => {
      for (const kind of CONTAINMENT_EDGE_KINDS) {
        useCodeGraphStore.getState().toggleEdgeKind(kind);
        // The filter map may not even have an entry, but toggling
        // must NOT create or flip one to true.
        const val = useCodeGraphStore.getState().edgeKindFilters[kind];
        expect(val).toBeFalsy();
      }
    });

    it("setEdgeKindEnabled is a no-op for containment edge kinds", () => {
      for (const kind of CONTAINMENT_EDGE_KINDS) {
        useCodeGraphStore.getState().setEdgeKindEnabled(kind, true);
        expect(useCodeGraphStore.getState().edgeKindFilters[kind]).toBeFalsy();
      }
    });
  });

  describe("DOI focus model", () => {
    it("sets and clears an explicit focus anchor", () => {
      useCodeGraphStore.getState().setFocusAnchor("symbol:foo");
      expect(useCodeGraphStore.getState().focusAnchorId).toBe("symbol:foo");
      useCodeGraphStore.getState().clearFocusAnchor();
      expect(useCodeGraphStore.getState().focusAnchorId).toBeNull();
    });

    it("stores and clears server impact samples for DOI focus", () => {
      useCodeGraphStore.getState().setDoiImpact(["a", "b", "a"]);
      expect(useCodeGraphStore.getState().doiImpactIds.size).toBe(2);
      useCodeGraphStore.getState().clearDoiImpact();
      expect(useCodeGraphStore.getState().doiImpactIds.size).toBe(0);
    });

    it("sets the focus direction", () => {
      useCodeGraphStore.getState().setFocusDirection("dependencies");
      expect(useCodeGraphStore.getState().focusDirection).toBe("dependencies");
      useCodeGraphStore.getState().setFocusDirection("dependents");
      expect(useCodeGraphStore.getState().focusDirection).toBe("dependents");
      useCodeGraphStore.getState().setFocusDirection("both");
      expect(useCodeGraphStore.getState().focusDirection).toBe("both");
    });

    it("clamps and rounds the DOI reveal count", () => {
      useCodeGraphStore.getState().setDoiRevealCount(0);
      expect(useCodeGraphStore.getState().doiRevealCount).toBe(
        MIN_DOI_REVEAL_COUNT,
      );
      useCodeGraphStore.getState().setDoiRevealCount(999);
      expect(useCodeGraphStore.getState().doiRevealCount).toBe(
        MAX_DOI_REVEAL_COUNT,
      );
      useCodeGraphStore.getState().setDoiRevealCount(42.6);
      expect(useCodeGraphStore.getState().doiRevealCount).toBe(43);
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

  describe("crateFilter", () => {
    it("defaults to null, sets and clears the isolated crate", () => {
      expect(useCodeGraphStore.getState().crateFilter).toBeNull();
      useCodeGraphStore.getState().setCrateFilter("djinn-graph");
      expect(useCodeGraphStore.getState().crateFilter).toBe("djinn-graph");
      useCodeGraphStore.getState().setCrateFilter(null);
      expect(useCodeGraphStore.getState().crateFilter).toBeNull();
    });

    it("reset clears the crate filter", () => {
      useCodeGraphStore.getState().setCrateFilter("ui");
      useCodeGraphStore.getState().reset();
      expect(useCodeGraphStore.getState().crateFilter).toBeNull();
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

    it("applyLens keeps complexity in lenses that show files or functions", () => {
      // Calls shows functions, Architecture shows files (colored by their
      // worst function) — both keep the heatmap engaged.
      useCodeGraphStore.getState().applyLens("calls");
      useCodeGraphStore.getState().setColorMode("complexity");
      expect(useCodeGraphStore.getState().colorMode).toBe("complexity");
      useCodeGraphStore.getState().applyLens("architecture");
      expect(useCodeGraphStore.getState().colorMode).toBe("complexity");
    });

    it("applyLens snaps complexity to topology for a lens with neither files nor functions", () => {
      useCodeGraphStore.getState().applyLens("calls");
      useCodeGraphStore.getState().setColorMode("complexity");
      // Types shows classes/structs (no functions) and no file nodes → the
      // heatmap would paint nothing, so the mode snaps back to topology.
      useCodeGraphStore.getState().applyLens("types");
      expect(useCodeGraphStore.getState().colorMode).toBe("topology");
    });

    it("applyLens leaves topology color mode untouched", () => {
      useCodeGraphStore.getState().setColorMode("topology");
      useCodeGraphStore.getState().applyLens("architecture");
      expect(useCodeGraphStore.getState().colorMode).toBe("topology");
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
      s.setFocusAnchor("foo");
      s.setFocusDirection("dependencies");
      s.setDoiRevealCount(MIN_DOI_REVEAL_COUNT);

      useCodeGraphStore.getState().reset();
      const after = useCodeGraphStore.getState();
      expect(after.selectionId).toBeNull();
      expect(after.hoverId).toBeNull();
      expect(after.citationIds.size).toBe(0);
      expect(after.toolHighlightIds.size).toBe(0);
      expect(after.blastRadiusFrontier.size).toBe(0);
      expect(after.focusAnchorId).toBeNull();
      expect(after.focusDirection).toBe(DEFAULT_FOCUS_DIRECTION);
      expect(after.doiRevealCount).toBe(DEFAULT_DOI_REVEAL_COUNT);
      expect(after.edgeKindFilters.Implements).toBe(true);
      expect(after.edgeKindFilters.Reads).toBe(false);
      expect(after.edgeKindFilters.FileReference).toBe(false);
      expect(after.activeLens).toBe("architecture");
      expect(after.colorMode).toBe("topology");
      expect(after.complexityAvailable).toBe(false);
      expect(after.selectedWorkspaceSlug).toBe("api");
    });
  });
});
