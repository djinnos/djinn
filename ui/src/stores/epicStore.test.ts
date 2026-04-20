import { beforeEach, describe, expect, it } from "vitest";
import { epicStore } from "./epicStore";
import { mockEpicA, mockEpicB } from "@/test/fixtures";

describe("epicStore", () => {
  beforeEach(() => {
    epicStore.getState().clearEpics();
  });

  it("addEpic adds an epic", () => {
    epicStore.getState().addEpic(mockEpicA);

    expect(epicStore.getState().getEpic(mockEpicA.id)).toEqual(mockEpicA);
  });

  it("updateEpic updates an existing epic", () => {
    epicStore.getState().addEpic(mockEpicA);
    const updated = { ...mockEpicA, title: "Updated Epic" };

    epicStore.getState().updateEpic(updated);

    expect(epicStore.getState().getEpic(mockEpicA.id)).toEqual(updated);
  });

  it("updateEpic does nothing when epic does not exist", () => {
    const updated = { ...mockEpicA, title: "Updated Epic" };

    epicStore.getState().updateEpic(updated);

    expect(epicStore.getState().getAllEpics()).toEqual([]);
  });

  it("removeEpic removes an epic", () => {
    epicStore.getState().addEpic(mockEpicA);

    epicStore.getState().removeEpic(mockEpicA.id);

    expect(epicStore.getState().getEpic(mockEpicA.id)).toBeUndefined();
  });

  it("setEpics replaces all epics", () => {
    epicStore.getState().addEpic(mockEpicA);

    epicStore.getState().setEpics([mockEpicB]);

    expect(epicStore.getState().getAllEpics()).toEqual([mockEpicB]);
  });

  it("clearEpics empties the epic map", () => {
    epicStore.getState().setEpics([mockEpicA, mockEpicB]);

    epicStore.getState().clearEpics();

    expect(epicStore.getState().getAllEpics()).toEqual([]);
  });

  it("getEpicsByStatus filters epics by status", () => {
    epicStore.getState().setEpics([mockEpicA, mockEpicB]);

    const openEpics = epicStore.getState().getEpicsByStatus("open");

    expect(openEpics).toEqual([mockEpicA]);
  });
});
