import { describe, expect, it } from "vitest";

import {
  EM_DASH,
  formatAverageReopens,
  formatCompactNumber,
  formatCurrency,
  formatDeltaPercent,
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
    expect(formatDeltaPercent(0.125)).toBe("+12%");
    expect(formatDeltaPercent(-0.025)).toBe("-2.5%");
    expect(formatDeltaPercent(null)).toBe(EM_DASH);
  });
});
