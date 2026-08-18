/**
 * TEMPORARY — proves the `UI Frontend` job's test step can go red.
 * Removed in the next commit on this branch.
 */
import { describe, expect, it } from "vitest";

describe("73h8 CI gate probe", () => {
  it("deliberately fails so the UI Frontend job must report red", () => {
    expect(1).toBe(2);
  });
});
