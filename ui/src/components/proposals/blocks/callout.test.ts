import { describe, expect, it } from "vitest";

import {
  CALLOUT_TONES,
  DEFAULT_CALLOUT_TONE,
  calloutToneAccent,
  normalizeCalloutTone,
} from "./callout";

describe("normalizeCalloutTone", () => {
  it("returns each known tone unchanged", () => {
    for (const tone of CALLOUT_TONES) {
      expect(normalizeCalloutTone(tone)).toBe(tone);
    }
  });

  it("is case-insensitive and whitespace-tolerant", () => {
    expect(normalizeCalloutTone("  WARNING ")).toBe("warning");
    expect(normalizeCalloutTone("Success")).toBe("success");
  });

  it("falls back to the default tone for missing / unknown values", () => {
    expect(normalizeCalloutTone(undefined)).toBe(DEFAULT_CALLOUT_TONE);
    expect(normalizeCalloutTone("")).toBe(DEFAULT_CALLOUT_TONE);
    expect(normalizeCalloutTone("bogus")).toBe(DEFAULT_CALLOUT_TONE);
  });

  it("defaults to info", () => {
    expect(DEFAULT_CALLOUT_TONE).toBe("info");
  });
});

describe("calloutToneAccent", () => {
  it("maps each tone to the documented shared-palette accent", () => {
    expect(calloutToneAccent("info")).toBe("blue");
    expect(calloutToneAccent("decision")).toBe("violet");
    expect(calloutToneAccent("risk")).toBe("red");
    expect(calloutToneAccent("warning")).toBe("amber");
    expect(calloutToneAccent("success")).toBe("emerald");
  });
});
