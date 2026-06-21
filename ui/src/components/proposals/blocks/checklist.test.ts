import { describe, expect, it } from "vitest";

import { parseChecklist } from "./checklist";

describe("parseChecklist — task lists", () => {
  it("parses checked / unchecked GFM task items", () => {
    const body = [
      "- [x] Ship the migration",
      "- [ ] Backfill the column",
      "- [X] Update the docs",
    ].join("\n");

    const { items, done, total } = parseChecklist(body);
    expect(total).toBe(3);
    expect(done).toBe(2);
    expect(items.map((i) => i.checked)).toEqual([true, false, true]);
    expect(items.map((i) => i.label)).toEqual([
      "Ship the migration",
      "Backfill the column",
      "Update the docs",
    ]);
  });

  it("treats plain bullets as unchecked items", () => {
    const { items, done } = parseChecklist("- first\n* second\n+ third");
    expect(items).toHaveLength(3);
    expect(done).toBe(0);
    expect(items.every((i) => !i.checked)).toBe(true);
  });

  it("splits a trailing note off the label", () => {
    const body = [
      "- [x] Done thing — with an em-dash note",
      "- [ ] Todo thing -- with a double-hyphen note",
      "- [ ] Spaced  with a double-space note",
    ].join("\n");

    const { items } = parseChecklist(body);
    expect(items[0]).toEqual({
      checked: true,
      label: "Done thing",
      note: "with an em-dash note",
    });
    expect(items[1].note).toBe("with a double-hyphen note");
    expect(items[2].note).toBe("with a double-space note");
  });

  it("ignores non-bullet lines and blank lines", () => {
    const body = "Some intro prose\n\n- [x] only this counts\nmore prose";
    const { items, total } = parseChecklist(body);
    expect(total).toBe(1);
    expect(items[0].label).toBe("only this counts");
  });
});

describe("parseChecklist — fallback", () => {
  it("returns an empty checklist for prose with no bullets", () => {
    expect(parseChecklist("No list here.")).toEqual({
      items: [],
      done: 0,
      total: 0,
    });
    expect(parseChecklist("")).toEqual({ items: [], done: 0, total: 0 });
  });
});
