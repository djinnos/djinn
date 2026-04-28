import { beforeEach, describe, expect, it } from "vitest";
import { useCodeGraphStore } from "./codeGraphStore";

beforeEach(() => {
  useCodeGraphStore.setState({
    selectionId: null,
    citationIds: new Set(),
  });
});

describe("codeGraphStore", () => {
  it("starts empty", () => {
    const s = useCodeGraphStore.getState();
    expect(s.selectionId).toBeNull();
    expect(s.citationIds.size).toBe(0);
  });

  it("setSelection updates the persistent selection", () => {
    useCodeGraphStore.getState().setSelection("file:src/foo.rs");
    expect(useCodeGraphStore.getState().selectionId).toBe("file:src/foo.rs");
  });

  it("setCitations replaces the active set and pins the first id", () => {
    useCodeGraphStore.getState().setCitations(["a", "b", "c"]);
    const s = useCodeGraphStore.getState();
    expect(s.citationIds).toEqual(new Set(["a", "b", "c"]));
    expect(s.selectionId).toBe("a");
  });

  it("setCitations with empty array clears selection", () => {
    useCodeGraphStore.getState().setCitations(["a"]);
    useCodeGraphStore.getState().setCitations([]);
    expect(useCodeGraphStore.getState().citationIds.size).toBe(0);
    expect(useCodeGraphStore.getState().selectionId).toBeNull();
  });

  it("addCitation merges into the active set without disturbing selection", () => {
    useCodeGraphStore.getState().setCitations(["a"]);
    useCodeGraphStore.getState().addCitation("b");
    const s = useCodeGraphStore.getState();
    expect(s.citationIds).toEqual(new Set(["a", "b"]));
    expect(s.selectionId).toBe("a"); // unchanged
  });

  it("addCitation is a no-op when the id is already present", () => {
    useCodeGraphStore.getState().setCitations(["a"]);
    const before = useCodeGraphStore.getState().citationIds;
    useCodeGraphStore.getState().addCitation("a");
    // Reference equality preserved for the no-op branch.
    expect(useCodeGraphStore.getState().citationIds).toBe(before);
  });

  it("clearCitations resets both the citation set and the selection", () => {
    useCodeGraphStore.getState().setCitations(["a"]);
    useCodeGraphStore.getState().clearCitations();
    const s = useCodeGraphStore.getState();
    expect(s.citationIds.size).toBe(0);
    expect(s.selectionId).toBeNull();
  });
});
