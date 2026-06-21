import { describe, expect, it } from "vitest";

import {
  ACCENT_BADGE,
  CHANGE_BADGE,
  CHANGE_NAME_INK,
  httpMethodColor,
  paramLocationColor,
  statusCodeColor,
} from "./blockBadgeColors";

describe("httpMethodColor", () => {
  it("maps each known verb to its accent surface", () => {
    expect(httpMethodColor("GET")).toBe(ACCENT_BADGE.emerald);
    expect(httpMethodColor("POST")).toBe(ACCENT_BADGE.blue);
    expect(httpMethodColor("PUT")).toBe(ACCENT_BADGE.amber);
    expect(httpMethodColor("PATCH")).toBe(ACCENT_BADGE.violet);
    expect(httpMethodColor("DELETE")).toBe(ACCENT_BADGE.red);
    expect(httpMethodColor("HEAD")).toBe(ACCENT_BADGE.slate);
    expect(httpMethodColor("OPTIONS")).toBe(ACCENT_BADGE.slate);
  });

  it("falls back to neutral slate for an unknown verb", () => {
    expect(httpMethodColor(undefined)).toBe(ACCENT_BADGE.slate);
  });
});

describe("statusCodeColor", () => {
  it("maps each status class to its accent: 2xx emerald / 4xx amber / 5xx red", () => {
    expect(statusCodeColor("success")).toBe(ACCENT_BADGE.emerald);
    expect(statusCodeColor("warn")).toBe(ACCENT_BADGE.amber);
    expect(statusCodeColor("error")).toBe(ACCENT_BADGE.red);
    expect(statusCodeColor("info")).toBe(ACCENT_BADGE.slate);
  });
});

describe("paramLocationColor", () => {
  it("maps IN-location: path violet / query blue / header amber / body emerald", () => {
    expect(paramLocationColor("path")).toBe(ACCENT_BADGE.violet);
    expect(paramLocationColor("query")).toBe(ACCENT_BADGE.blue);
    expect(paramLocationColor("header")).toBe(ACCENT_BADGE.amber);
    expect(paramLocationColor("body")).toBe(ACCENT_BADGE.emerald);
  });
});

describe("change vocabulary", () => {
  it("keys A/M/D/R chips to added emerald / modified blue / deleted red / renamed violet", () => {
    expect(CHANGE_BADGE.added).toBe(ACCENT_BADGE.emerald);
    expect(CHANGE_BADGE.modified).toBe(ACCENT_BADGE.blue);
    expect(CHANGE_BADGE.deleted).toBe(ACCENT_BADGE.red);
    expect(CHANGE_BADGE.renamed).toBe(ACCENT_BADGE.violet);
  });

  it("strikes through the deleted name ink", () => {
    expect(CHANGE_NAME_INK.deleted).toContain("line-through");
    expect(CHANGE_NAME_INK.added).toBe("text-emerald-300");
  });
});
