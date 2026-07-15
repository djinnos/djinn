import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";

import { CoverageGapBanner } from "./CoverageGapBanner";
import type { CodeGraphCoverage } from "@/api/codeGraph";

describe("CoverageGapBanner", () => {
  it("names unindexed workspaces when coverage has gaps", () => {
    const coverage: CodeGraphCoverage = {
      hasGaps: true,
      gaps: [
        { slug: "server", language: "rust", status: "timed_out" },
        { slug: "legacy", language: "ruby", status: "unsupported_language" },
      ],
    };
    render(<CoverageGapBanner coverage={coverage} />);

    const banner = screen.getByRole("status", { name: /index coverage gap/i });
    expect(banner).toHaveTextContent("2 workspaces not indexed");
    expect(banner).toHaveTextContent("server (rust)");
    expect(banner).toHaveTextContent("legacy (ruby)");
  });

  it("renders nothing when coverage is clean", () => {
    const { container } = render(
      <CoverageGapBanner coverage={{ hasGaps: false, gaps: [] }} />,
    );
    expect(container).toBeEmptyDOMElement();
  });

  it("renders nothing when coverage is null", () => {
    const { container } = render(<CoverageGapBanner coverage={null} />);
    expect(container).toBeEmptyDOMElement();
  });
});
