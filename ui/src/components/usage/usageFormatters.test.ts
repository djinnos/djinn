import { describe, expect, it } from "vitest";

import {
  EM_DASH,
  formatAverageReopens,
  formatCompactNumber,
  formatCurrency,
  formatDeltaPercent,
  formatInteger,
  formatPercent,
} from "./usageFormatters";

describe("usage formatters", () => {
  it("renders null costs as an em dash and zero costs as priced zero", () => {
    expect(formatCurrency(null)).toBe(EM_DASH);
    expect(formatCurrency(undefined)).toBe(EM_DASH);
    expect(formatCurrency(0)).toBe("$0.00");
  });

  it("formats tokens, percentages, reopens, and deltas consistently", () => {
    expect(formatCompactNumber(1234)).toBe("1.2K");
    expect(formatPercent(0.875)).toBe("88%");
    expect(formatAverageReopens(1.234)).toBe("1.23");
    expect(formatDeltaPercent(0.125)).toBe("+13%");
    expect(formatDeltaPercent(-0.025)).toBe("-2.5%");
    expect(formatDeltaPercent(null)).toBe(EM_DASH);
  });

  it("formats integers without decimals", () => {
    expect(formatInteger(0)).toBe("0");
    expect(formatInteger(42)).toBe("42");
    expect(formatInteger(null)).toBe(EM_DASH);
    expect(formatInteger(undefined)).toBe(EM_DASH);
  });

  it("formats currency values for split cost-basis fields", () => {
    // Actual API spend: $0 is a valid priced zero (API-key sessions with zero cost).
    expect(formatCurrency(0)).toBe("$0.00");

    // Projected subscription cost: non-zero values format correctly.
    expect(formatCurrency(12.5)).toBe("$12.50");
    expect(formatCurrency(150)).toBe("$150");

    // Null/unset split fields render as em dash (not $0).
    expect(formatCurrency(null)).toBe(EM_DASH);
    expect(formatCurrency(undefined)).toBe(EM_DASH);
  });
});
