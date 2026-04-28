import { describe, expect, it } from "vitest";

import {
  buildImpactMermaid,
  type ImpactDetailedResult,
} from "@/components/codegraph/impactMermaid";

const detailedFixture: ImpactDetailedResult = {
  key: "scip-rust . my_crate v1 src/lib.rs#do_thing()",
  target_label: "do_thing",
  entries: [
    { key: "scip-rust . my_crate v1 src/a.rs#caller_a()", depth: 1 },
    { key: "scip-rust . my_crate v1 src/a.rs#caller_b()", depth: 1 },
    { key: "scip-rust . my_crate v1 src/b.rs#deep_caller()", depth: 2 },
    {
      key: "scip-rust . my_crate v1 src/c.rs#deeper()",
      depth: 3,
      display_name: "deeper",
    },
  ],
  risk: "HIGH",
  summary: "2 direct caller(s) across 3 module(s)",
};

describe("buildImpactMermaid", () => {
  it("starts with a flowchart TD header and a target node", () => {
    const out = buildImpactMermaid(detailedFixture);
    const [header, ...rest] = out.split("\n");
    expect(header).toBe("flowchart TD");
    expect(rest[0]).toContain('target["do_thing"]');
    expect(out).toContain("classDef target");
  });

  it("buckets entries into depth subgraphs in ascending depth order", () => {
    const out = buildImpactMermaid(detailedFixture);
    const directIdx = out.indexOf('subgraph depth_1["Direct (depth 1)"]');
    const depth2Idx = out.indexOf('subgraph depth_2["Depth 2"]');
    const depth3Idx = out.indexOf('subgraph depth_3["Depth 3"]');
    expect(directIdx).toBeGreaterThan(0);
    expect(depth2Idx).toBeGreaterThan(directIdx);
    expect(depth3Idx).toBeGreaterThan(depth2Idx);
  });

  it("emits one node per entry inside its depth bucket", () => {
    const out = buildImpactMermaid(detailedFixture);
    // trimKey preserves the trailing `()` that SCIP function keys carry —
    // we only strip the file/scope prefix, not the call-marker.
    expect(out).toContain('n0["caller_a()"]');
    expect(out).toContain('n1["caller_b()"]');
    expect(out).toContain('n2["deep_caller()"]');
    // Custom display_name overrides trimKey output (no trailing parens).
    expect(out).toContain('n3["deeper"]');
  });

  it("wires depth-1 entries to the target and chains deeper entries to a shallower bucket", () => {
    const out = buildImpactMermaid(detailedFixture);
    expect(out).toContain("n0 --> target");
    expect(out).toContain("n1 --> target");
    // Depth-2 caller points at the *first* depth-1 entry (n0).
    expect(out).toContain("n2 --> n0");
    // Depth-3 entry points at the first depth-2 entry (n2).
    expect(out).toContain("n3 --> n2");
  });

  it("renders an empty state with just the target node when there are no entries", () => {
    const out = buildImpactMermaid({
      key: "scip-rust . my_crate v1 src/lib.rs#lonely()",
      entries: [],
      risk: "LOW",
      summary: "no direct callers in current graph snapshot",
    });
    expect(out).toBe(
      [
        "flowchart TD",
        '  target["lonely()"]:::target',
        "classDef target fill:#fde68a,stroke:#b45309;",
      ].join("\n"),
    );
  });

  it("escapes double quotes in labels so Mermaid can parse them", () => {
    const out = buildImpactMermaid({
      key: 'scip-rust . crate v1 src/x.rs#weird "name"()',
      target_label: 'weird "name"',
      entries: [],
    });
    // No literal " in the inner label — replaced with single quote.
    expect(out).not.toContain('"weird "name""');
    expect(out).toContain("weird 'name'");
  });

  it("preserves entries when depths have gaps", () => {
    // The server may filter intermediate entries via `min_confidence`. We
    // still want every supplied entry on the chart, with deeper buckets
    // chaining at the closest shallower depth available.
    const out = buildImpactMermaid({
      key: "scip-rust . crate v1 src/x.rs#root()",
      entries: [
        { key: "scip-rust . crate v1 src/a.rs#a()", depth: 1 },
        // depth 2 missing
        { key: "scip-rust . crate v1 src/c.rs#c()", depth: 3 },
      ],
    });
    expect(out).toContain("n0 --> target");
    // n1 (depth 3) should point at the closest shallower bucket: n0 (depth 1).
    expect(out).toContain("n1 --> n0");
  });
});
