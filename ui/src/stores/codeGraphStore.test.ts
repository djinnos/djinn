import { beforeEach, describe, expect, it } from "vitest";

import {
  DEFAULT_DEPTH,
  EDGE_KINDS,
  MAX_DEPTH,
  MIN_DEPTH,
  useCodeGraphStore,
} from "./codeGraphStore";

describe("codeGraphStore", () => {
  beforeEach(() => {
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

    it("starts with non-noisy edge kinds enabled and SymbolReference/MemberOf off", () => {
      const filters = useCodeGraphStore.getState().edgeKindFilters;
      for (const kind of EDGE_KINDS) {
        const expected = !(kind === "SymbolReference" || kind === "MemberOf");
        expect(filters[kind]).toBe(expected);
      }
    });

    it("starts with all top-level node kinds visible", () => {
      const filters = useCodeGraphStore.getState().nodeKindFilters;
      expect(filters.folder).toBe(true);
      expect(filters.file).toBe(true);
      expect(filters.symbol).toBe(true);
    });

    it("starts with noisy symbol kinds (variable/import) hidden", () => {
      const filters = useCodeGraphStore.getState().symbolKindFilters;
      expect(filters.function).toBe(true);
      expect(filters.class).toBe(true);
      expect(filters.variable).toBe(false);
      expect(filters.import).toBe(false);
    });

    it("starts at the default (max) depth", () => {
      expect(useCodeGraphStore.getState().depthFilter).toBe(DEFAULT_DEPTH);
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
      useCodeGraphStore.getState().toggleEdgeKind("Reads");
      expect(useCodeGraphStore.getState().edgeKindFilters.Reads).toBe(false);
      useCodeGraphStore.getState().toggleEdgeKind("Reads");
      expect(useCodeGraphStore.getState().edgeKindFilters.Reads).toBe(true);
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

  describe("reset", () => {
    it("returns every slice to its default", () => {
      const s = useCodeGraphStore.getState();
      s.setSelection("foo");
      s.setCitations(["a"]);
      s.setToolHighlight(["b"]);
      s.setBlastRadiusFrontier(["c"]);
      s.setHover("foo");
      s.toggleEdgeKind("Reads");
      s.setDepthFilter(1);

      useCodeGraphStore.getState().reset();
      const after = useCodeGraphStore.getState();
      expect(after.selectionId).toBeNull();
      expect(after.hoverId).toBeNull();
      expect(after.citationIds.size).toBe(0);
      expect(after.toolHighlightIds.size).toBe(0);
      expect(after.blastRadiusFrontier.size).toBe(0);
      expect(after.depthFilter).toBe(DEFAULT_DEPTH);
      expect(after.edgeKindFilters.Reads).toBe(true);
    });
  });
});
