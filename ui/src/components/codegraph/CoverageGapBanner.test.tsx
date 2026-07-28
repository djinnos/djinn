import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";

import { CoverageGapBanner } from "./CoverageGapBanner";
import { relativeAge } from "./coverageAdvisory";
import type { CodeGraphCoverage, CodeGraphWorkspace } from "@/api/codeGraph";

/** 2026-07-28T14:06:50Z — the warm attempt the fixtures below describe. */
const ATTEMPT = "2026-07-28T14:06:50.434Z";
/** Two hours after ATTEMPT. */
const NOW = Date.parse("2026-07-28T16:10:00.000Z");

describe("CoverageGapBanner", () => {
  it("reports a timed-out workspace as stale, naming the commit still on screen", () => {
    const coverage: CodeGraphCoverage = {
      hasGaps: true,
      gaps: [
        {
          slug: "server",
          language: "rust",
          status: "timed_out",
          attemptedAt: ATTEMPT,
          discoveredFiles: 1439,
          indexedFiles: 0,
        },
      ],
    };
    // `server` carries nodes from an older commit than its healthy peers —
    // that difference is the staleness.
    const workspaces: CodeGraphWorkspace[] = [
      { slug: "server", nodeCount: 1457, commitSha: "f355d753b735" },
      { slug: "ui", nodeCount: 19367, commitSha: "068b947fb081" },
      { slug: "website", nodeCount: 170, commitSha: "068b947fb081" },
    ];
    render(
      <CoverageGapBanner
        coverage={coverage}
        workspaces={workspaces}
        now={NOW}
      />,
    );

    const banner = screen.getByRole("status", { name: /index coverage gap/i });
    expect(banner).toHaveTextContent("server (rust)");
    expect(banner).toHaveTextContent("index timed out 2h ago");
    expect(banner).toHaveTextContent("showing f355d75");
    // The claim the old copy made and could not support.
    expect(banner).not.toHaveTextContent("not indexed");
  });

  it("says not-in-the-graph only when the workspace really contributes nothing", () => {
    const coverage: CodeGraphCoverage = {
      hasGaps: true,
      gaps: [
        {
          slug: "legacy",
          language: "ruby",
          status: "unsupported_language",
          attemptedAt: ATTEMPT,
        },
      ],
    };
    render(
      <CoverageGapBanner
        coverage={coverage}
        workspaces={[{ slug: "legacy", nodeCount: 0 }]}
        now={NOW}
      />,
    );

    const banner = screen.getByRole("status", { name: /index coverage gap/i });
    expect(banner).toHaveTextContent("legacy (ruby): no indexer");
    expect(banner).toHaveTextContent("not in the graph");
    // A permanent gap must not promise a retry that will never come.
    expect(banner.getAttribute("title")).toContain(
      "will not resolve on its own",
    );
  });

  it("tells a retriable gap apart from a permanent one in the tooltip", () => {
    render(
      <CoverageGapBanner
        coverage={{
          hasGaps: true,
          gaps: [
            {
              slug: "server",
              language: "rust",
              status: "timed_out",
              discoveredFiles: 1439,
            },
          ],
        }}
        workspaces={[
          { slug: "server", nodeCount: 1457, commitSha: "aaa" },
          { slug: "ui", nodeCount: 10, commitSha: "bbb" },
        ]}
        now={NOW}
      />,
    );
    const title =
      screen
        .getByRole("status", { name: /index coverage gap/i })
        .getAttribute("title") ?? "";
    expect(title).toContain("re-attempts on its own schedule");
    expect(title).toContain("0 of 1439 files");
    expect(title).toContain("grep");
  });

  it("falls back to the alarming reading when no workspace data is available", () => {
    render(
      <CoverageGapBanner
        coverage={{
          hasGaps: true,
          gaps: [
            { slug: "server", language: "rust", status: "indexer_failed" },
          ],
        }}
        now={NOW}
      />,
    );
    const banner = screen.getByRole("status", { name: /index coverage gap/i });
    expect(banner).toHaveTextContent("indexer failed");
    expect(banner).toHaveTextContent("not in the graph");
  });

  it("lists several gaps compactly", () => {
    render(
      <CoverageGapBanner
        coverage={{
          hasGaps: true,
          gaps: [
            { slug: "server", language: "rust", status: "timed_out" },
            {
              slug: "legacy",
              language: "ruby",
              status: "unsupported_language",
            },
          ],
        }}
        workspaces={[
          { slug: "server", nodeCount: 1457, commitSha: "aaa" },
          { slug: "ui", nodeCount: 10, commitSha: "bbb" },
        ]}
        now={NOW}
      />,
    );
    const banner = screen.getByRole("status", { name: /index coverage gap/i });
    expect(banner).toHaveTextContent("2 workspaces degraded");
    expect(banner).toHaveTextContent("server (rust, index timed out, stale)");
    expect(banner).toHaveTextContent("legacy (ruby, no indexer, not in graph)");
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

describe("relativeAge", () => {
  const now = Date.parse("2026-07-28T16:00:00.000Z");

  it("renders each magnitude compactly", () => {
    expect(relativeAge("2026-07-28T15:59:30.000Z", now)).toBe("30s");
    expect(relativeAge("2026-07-28T15:30:00.000Z", now)).toBe("30m");
    expect(relativeAge("2026-07-28T14:00:00.000Z", now)).toBe("2h");
    expect(relativeAge("2026-07-25T16:00:00.000Z", now)).toBe("3d");
  });

  it("omits the clause rather than inventing one", () => {
    expect(relativeAge(undefined, now)).toBeUndefined();
    expect(relativeAge("not a date", now)).toBeUndefined();
  });
});
