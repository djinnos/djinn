import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import {
  PROPOSAL_BLOCK_REGISTRY,
  getProposalBlockDefinitionByTag,
} from "@/lib/proposalBlocks";

import { BLOCK_COMPONENTS, BLOCK_DISPLAY_NAMES } from "@/lib/blockRegistry";

import {
  extractBlockTags,
  isPascalCaseTag,
  parseMdxBody,
} from "./parseMdxBody";
import { extractProposalBlockIds } from "./blockRegistry";
import { PROPOSAL_BLOCK_SPECS } from "./registry";

// ------------------------------------------------------------------------------
// Canonical v1 tag set — DERIVED from the Rust-emitted block catalog.
//
// The literal tag list used to live here, which made it a place where the TS
// and Rust block sets could silently diverge. Instead we read the committed
// catalog JSON that the Rust registry emits:
//
//   server/crates/djinn-control-plane/src/tools/proposal_blocks/
//     proposal_block_catalog.json
//
// A Rust test (`canonical_catalog_json_is_in_sync`) fails in CI if that file
// drifts from the Rust registry, so the JSON is a trustworthy mirror of the
// Rust source of truth. By reading it here, adding a block now requires
// updating BOTH languages (or CI fails loudly on one side or the other).
//
// We read it with plain `fs.readFileSync` and an absolute path resolved from
// `import.meta.url` — NOT a bundler JSON import — because the file lives
// outside the `ui/` root and a Vite module import could be blocked by the dev
// server's `fs.allow` sandbox. A direct `fs` read is unaffected by that.
//
// ORDERING CONTRACT: the catalog JSON is sorted by block `type`, but several
// existing tests assert against FIXTURE order (the order blocks appear in the
// canonical MDX fixture, which equals `PROPOSAL_BLOCK_SPECS` order with
// `QuestionForm` LAST). So we take the authoritative tag *set* from the catalog
// and *order* it by the spec/fixture order. A drift assertion below proves the
// catalog set and the spec set are identical, so the chosen ordering can never
// hide a missing/extra tag.
// ------------------------------------------------------------------------------

const TEST_DIR = dirname(fileURLToPath(import.meta.url));

const CATALOG_PATH = resolve(
  TEST_DIR,
  "../../../../..",
  "server/crates/djinn-control-plane/src/tools/proposal_blocks/proposal_block_catalog.json",
);

interface CatalogEntry {
  type: string;
  tag: string;
}

const CATALOG: readonly CatalogEntry[] = JSON.parse(
  readFileSync(CATALOG_PATH, "utf8"),
) as CatalogEntry[];

// Authoritative tag SET from the Rust catalog (catalog is sorted by `type`).
const CATALOG_TAGS: readonly string[] = CATALOG.map((entry) => entry.tag);
const CATALOG_TAG_SET = new Set(CATALOG_TAGS);
// Authoritative type SET from the Rust catalog. Mirrors CATALOG_TAGS but keyed
// by the stable Rust-side block `type` (e.g. "rich-text", "question-form").
const CATALOG_TYPES: readonly string[] = CATALOG.map((entry) => entry.type);

// Fixture/spec order (QuestionForm last), filtered to the catalog set so the
// ordered list and the authoritative set are kept in lockstep by construction.
const SPEC_TAG_ORDER = PROPOSAL_BLOCK_SPECS.map((spec) => spec.tag);
const SPEC_TYPE_ORDER = PROPOSAL_BLOCK_SPECS.map((spec) => spec.type);

export const CANONICAL_V1_TAGS = SPEC_TAG_ORDER.filter((tag) =>
  CATALOG_TAG_SET.has(tag),
) as readonly string[];

export type CanonicalV1Tag = (typeof CANONICAL_V1_TAGS)[number];

// ------------------------------------------------------------------------------
// Cross-language drift gate: the Rust catalog set and the TS spec set MUST be
// identical. Combined with the Rust-side `canonical_catalog_json_is_in_sync`
// test, this makes adding a proposal block require updating BOTH languages.
// ------------------------------------------------------------------------------

describe("Rust ⇄ TS block catalog drift gate", () => {
  it("reads the committed Rust block catalog JSON", () => {
    // Prove the file was actually read (not silently empty / mocked away).
    expect(CATALOG.length).toBeGreaterThan(0);
    expect(CATALOG_TAGS.every((tag) => typeof tag === "string")).toBe(true);
    expect(CATALOG_TYPES.every((type) => typeof type === "string")).toBe(true);
  });

  it("catalog entries have unique `type` and unique `tag` values", () => {
    // Drift-defense: if two catalog entries shared a `type` or `tag`, the
    // sorted-equality assertion below would silently collapse the duplicate
    // (both sides would dedupe by accident) and the drift gate would mask a
    // bug rather than fail. Catch duplicates BEFORE the set comparison.
    const types = CATALOG.map((entry) => entry.type);
    const tags = CATALOG.map((entry) => entry.tag);
    expect(new Set(types).size, "catalog `type` values must be unique").toBe(
      types.length,
    );
    expect(new Set(tags).size, "catalog `tag` values must be unique").toBe(
      tags.length,
    );
  });

  it("catalog tag set === TS spec tag set (neither side has drifted)", () => {
    const catalogSorted = [...CATALOG_TAGS].sort();
    const specSorted = [...SPEC_TAG_ORDER].sort();
    expect(
      specSorted,
      "TS PROPOSAL_BLOCK_SPECS tags must match the Rust block catalog exactly",
    ).toEqual(catalogSorted);
  });

  it("catalog type set === TS spec type set (neither side has drifted)", () => {
    // Symmetric to the tag check above: the Rust `proposal_block_registry`
    // and the JSON catalog emit a stable `type` per block, and the TS
    // `PROPOSAL_BLOCK_SPECS` MUST use the exact same type set. A mismatch
    // here means a block was added/removed/renamed on one side only — which
    // is exactly the drift we are trying to gate.
    const catalogSorted = [...CATALOG_TYPES].sort();
    const specSorted = [...SPEC_TYPE_ORDER].sort();
    expect(
      specSorted,
      "TS PROPOSAL_BLOCK_SPECS types must match the Rust block catalog exactly",
    ).toEqual(catalogSorted);
  });

  it("derived CANONICAL_V1_TAGS covers exactly the catalog set, in fixture order", () => {
    // Set equality (order-independent) against the authoritative catalog.
    expect([...CANONICAL_V1_TAGS].sort()).toEqual([...CATALOG_TAGS].sort());
    // No tag was dropped by the order-reconciliation filter.
    expect(CANONICAL_V1_TAGS).toHaveLength(CATALOG_TAGS.length);
    // Ordering contract: QuestionForm stays last.
    expect(CANONICAL_V1_TAGS[CANONICAL_V1_TAGS.length - 1]).toBe("QuestionForm");
  });
});

// Import the canonical fixture as a raw string so the test suite exercises
// the exact same MDX sample that backend validation tests use.
const CANONICAL_MDX = (await import("./__fixtures__/canonicalProposal.mdx?raw"))
  .default as string;

// ------------------------------------------------------------------------------
// Cross-language completeness: every TS structure and the canonical fixture
// must contain EXACTLY the Rust catalog tag set. These are phrased directly
// against `CATALOG_TAG_SET` (the Rust source of truth) rather than the derived
// `CANONICAL_V1_TAGS`, so they fail loudly if a block is added on one side only.
// ------------------------------------------------------------------------------

describe("Rust catalog completeness across TS structures", () => {
  const catalogSorted = [...CATALOG_TAGS].sort();

  it("PROPOSAL_BLOCK_REGISTRY tag set === Rust catalog tag set", () => {
    const tags = Object.values(PROPOSAL_BLOCK_REGISTRY)
      .map((def) => def.tag)
      .sort();
    expect(tags).toEqual(catalogSorted);
  });

  it("BLOCK_COMPONENTS keys === BLOCK_DISPLAY_NAMES keys === Rust catalog tag set", () => {
    const componentTags = Object.keys(BLOCK_COMPONENTS).sort();
    const displayNameTags = Object.keys(BLOCK_DISPLAY_NAMES).sort();
    expect(componentTags).toEqual(catalogSorted);
    expect(displayNameTags).toEqual(catalogSorted);
  });

  it("canonical MDX fixture exercises EXACTLY the Rust catalog tag set", () => {
    const fixtureTags = [...new Set(extractBlockTags(CANONICAL_MDX))].sort();
    expect(
      fixtureTags,
      "every catalog block must appear in canonicalProposal.mdx, and no extra",
    ).toEqual(catalogSorted);
  });
});

// ------------------------------------------------------------------------------
// Parity: canonical tag set
// ------------------------------------------------------------------------------

describe("canonical tag set parity", () => {
  it("PROPOSAL_BLOCK_REGISTRY contains exactly the canonical v1 tags", () => {
    const registryTags = Object.values(PROPOSAL_BLOCK_REGISTRY).map(
      (def) => def.tag,
    );
    expect(registryTags.sort()).toEqual([...CANONICAL_V1_TAGS].sort());
  });

  it("every canonical tag resolves to a registry definition", () => {
    for (const tag of CANONICAL_V1_TAGS) {
      const def = getProposalBlockDefinitionByTag(tag);
      expect(def, `${tag} should be in PROPOSAL_BLOCK_REGISTRY`).toBeDefined();
      expect(def!.tag).toBe(tag);
    }
  });

  it("no extra tags exist in the registry beyond the canonical set", () => {
    const registryTags = Object.values(PROPOSAL_BLOCK_REGISTRY).map(
      (def) => def.tag,
    );
    expect(registryTags).toHaveLength(CANONICAL_V1_TAGS.length);
    for (const tag of registryTags) {
      expect(
        CANONICAL_V1_TAGS.includes(tag as CanonicalV1Tag),
        `${tag} is not in the canonical v1 tag set`,
      ).toBe(true);
    }
  });
});

// ------------------------------------------------------------------------------
// Single-source completeness: the three derived structures must agree.
//
// `defineBlock()` makes every block one spec; `PROPOSAL_BLOCK_REGISTRY`,
// `BLOCK_COMPONENTS`, and `BLOCK_DISPLAY_NAMES` are all DERIVED from that single
// source. This guard fails loudly if a future block is only half-registered
// (e.g. a field schema with no component or display name).
// ------------------------------------------------------------------------------

describe("derived block structures completeness", () => {
  const canonical = [...CANONICAL_V1_TAGS].sort();

  it("PROPOSAL_BLOCK_REGISTRY tags === the canonical tags", () => {
    const tags = Object.values(PROPOSAL_BLOCK_REGISTRY).map((def) => def.tag);
    expect(tags).toHaveLength(CANONICAL_V1_TAGS.length);
    expect([...tags].sort()).toEqual(canonical);
  });

  it("BLOCK_COMPONENTS keys === the 17 canonical tags (each a component)", () => {
    const tags = Object.keys(BLOCK_COMPONENTS);
    expect(tags).toHaveLength(CANONICAL_V1_TAGS.length);
    expect([...tags].sort()).toEqual(canonical);
    for (const tag of tags) {
      expect(BLOCK_COMPONENTS[tag], `${tag} must have a component`).toBeTruthy();
    }
  });

  it("BLOCK_DISPLAY_NAMES keys === the 17 canonical tags (each a label)", () => {
    const tags = Object.keys(BLOCK_DISPLAY_NAMES);
    expect(tags).toHaveLength(CANONICAL_V1_TAGS.length);
    expect([...tags].sort()).toEqual(canonical);
    for (const tag of tags) {
      expect(
        typeof BLOCK_DISPLAY_NAMES[tag] === "string" &&
          BLOCK_DISPLAY_NAMES[tag].length > 0,
        `${tag} must have a non-empty display name`,
      ).toBe(true);
    }
  });

  it("all three derived structures share the exact same tag set", () => {
    const registryTags = Object.values(PROPOSAL_BLOCK_REGISTRY)
      .map((def) => def.tag)
      .sort();
    const componentTags = Object.keys(BLOCK_COMPONENTS).sort();
    const displayNameTags = Object.keys(BLOCK_DISPLAY_NAMES).sort();
    expect(componentTags).toEqual(registryTags);
    expect(displayNameTags).toEqual(registryTags);
  });

  it("specs preserve canonical order with QuestionForm LAST", () => {
    const specTags = PROPOSAL_BLOCK_SPECS.map((spec) => spec.tag);
    expect(specTags).toEqual([...CANONICAL_V1_TAGS]);
    expect(specTags[specTags.length - 1]).toBe("QuestionForm");
  });
});

// ------------------------------------------------------------------------------
// Round-trip: canonical MDX → parser → registry
// ------------------------------------------------------------------------------

describe("canonical proposal.mdx round-trip", () => {
  it("parses all canonical v1 block types from the canonical fixture", () => {
    const segments = parseMdxBody(CANONICAL_MDX);
    const blockSegments = segments.filter((s) => s.kind === "block");
    const extractedTags = blockSegments.map((s) => s.tag);

    expect(extractedTags).toEqual(CANONICAL_V1_TAGS);
  });

  it("every parsed block resolves to the TS registry definition", () => {
    const segments = parseMdxBody(CANONICAL_MDX);
    const blockSegments = segments.filter((s) => s.kind === "block");

    for (const segment of blockSegments) {
      const def = getProposalBlockDefinitionByTag(segment.tag);
      expect(
        def,
        `${segment.tag} should resolve to a registry definition`,
      ).toBeDefined();
      expect(def!.tag).toBe(segment.tag);
      expect(def!.type).toBeDefined();
    }
  });

  it("asserts parsed tag, block id, and registry mapping consistency for all v1 types", () => {
    const segments = parseMdxBody(CANONICAL_MDX);
    const blockSegments = segments.filter((s) => s.kind === "block");

    expect(blockSegments).toHaveLength(CANONICAL_V1_TAGS.length);

    // Build a map for easy lookup by tag
    const byTag = new Map(
      blockSegments.map((s) => [s.tag, s as (typeof blockSegments)[number]]),
    );

    // RichText
    const richText = byTag.get("RichText");
    expect(richText).toBeDefined();
    expect(richText!.id).toBe("intro");
    expect(richText!.content).toContain("rich text");
    expect(getProposalBlockDefinitionByTag("RichText")!.type).toBe("rich-text");

    // Diagram
    const diagram = byTag.get("Diagram");
    expect(diagram).toBeDefined();
    expect(diagram!.id).toBe("arch-overview");
    expect(diagram!.attributes.type).toBe("mermaid");
    expect(diagram!.content).toContain("graph TD");
    expect(getProposalBlockDefinitionByTag("Diagram")!.type).toBe("diagram");

    // AnnotatedCode
    const annotatedCode = byTag.get("AnnotatedCode");
    expect(annotatedCode).toBeDefined();
    expect(annotatedCode!.id).toBe("handler");
    expect(annotatedCode!.attributes.language).toBe("rust");
    expect(annotatedCode!.content).toContain("fn handle_request");
    expect(getProposalBlockDefinitionByTag("AnnotatedCode")!.type).toBe(
      "annotated-code",
    );

    // ApiEndpoint
    const apiEndpoint = byTag.get("ApiEndpoint");
    expect(apiEndpoint).toBeDefined();
    expect(apiEndpoint!.id).toBe("get-users");
    expect(apiEndpoint!.attributes.method).toBe("GET");
    expect(apiEndpoint!.attributes.path).toBe("/api/users");
    expect(apiEndpoint!.content).toContain("Returns a list");
    expect(getProposalBlockDefinitionByTag("ApiEndpoint")!.type).toBe(
      "api-endpoint",
    );

    // Decisions
    const decisions = byTag.get("Decisions");
    expect(decisions).toBeDefined();
    expect(decisions!.id).toBe("auth-choice");
    expect(decisions!.content).toContain("JWT");
    expect(getProposalBlockDefinitionByTag("Decisions")!.type).toBe(
      "decisions",
    );

    // FileTree
    const fileTree = byTag.get("FileTree");
    expect(fileTree).toBeDefined();
    expect(fileTree!.id).toBe("project-layout");
    expect(fileTree!.attributes.root).toBe("src");
    expect(fileTree!.content).toContain("main.rs");
    expect(getProposalBlockDefinitionByTag("FileTree")!.type).toBe("file-tree");

    // Diff
    const diff = byTag.get("Diff");
    expect(diff).toBeDefined();
    expect(diff!.id).toBe("add-fn");
    expect(diff!.attributes.filename).toBe("src/add.ts");
    expect(diff!.attributes.lang).toBe("ts");
    expect(diff!.content).toContain("@@");
    expect(getProposalBlockDefinitionByTag("Diff")!.type).toBe("diff");

    // Callout
    const callout = byTag.get("Callout");
    expect(callout).toBeDefined();
    expect(callout!.id).toBe("perf-note");
    expect(callout!.attributes.tone).toBe("warning");
    expect(callout!.content).toContain("hot path");
    expect(getProposalBlockDefinitionByTag("Callout")!.type).toBe("callout");

    // Checklist
    const checklist = byTag.get("Checklist");
    expect(checklist).toBeDefined();
    expect(checklist!.id).toBe("acceptance");
    expect(checklist!.content).toContain("[x]");
    expect(getProposalBlockDefinitionByTag("Checklist")!.type).toBe(
      "checklist",
    );

    // JsonExplorer
    const jsonExplorer = byTag.get("JsonExplorer");
    expect(jsonExplorer).toBeDefined();
    expect(jsonExplorer!.id).toBe("config-sample");
    expect(jsonExplorer!.content).toContain("\"enabled\"");
    expect(getProposalBlockDefinitionByTag("JsonExplorer")!.type).toBe(
      "json-explorer",
    );

    // Tabs
    const tabs = byTag.get("Tabs");
    expect(tabs).toBeDefined();
    expect(tabs!.id).toBe("walkthrough");
    expect(tabs!.attributes.tabs).toContain("Overview");
    expect(getProposalBlockDefinitionByTag("Tabs")!.type).toBe("tabs");

    // Columns
    const columns = byTag.get("Columns");
    expect(columns).toBeDefined();
    expect(columns!.id).toBe("before-after");
    expect(columns!.attributes.columns).toContain("body");
    expect(getProposalBlockDefinitionByTag("Columns")!.type).toBe("columns");

    // QuestionForm
    const questionForm = byTag.get("QuestionForm");
    expect(questionForm).toBeDefined();
    expect(questionForm!.id).toBe("open-questions");
    expect(questionForm!.attributes.title).toBe("Open Questions");
    expect(questionForm!.content).toContain("Redis");
    expect(getProposalBlockDefinitionByTag("QuestionForm")!.type).toBe(
      "question-form",
    );
  });

  it("markdown segments are preserved between blocks in the canonical fixture", () => {
    const segments = parseMdxBody(CANONICAL_MDX);
    const mdSegments = segments.filter((s) => s.kind === "markdown");

    expect(mdSegments.length).toBeGreaterThanOrEqual(2);
    expect(mdSegments[0].text).toContain("# Canonical Proposal");
    const last = mdSegments[mdSegments.length - 1];
    expect(last.text).toContain("trailing markdown");
  });
});

// ------------------------------------------------------------------------------
// Parser behaviour (not dependent on the canonical fixture)
// ------------------------------------------------------------------------------

describe("parseMdxBody — self-closing tags", () => {
  it("parses a self-closing <Diagram /> tag", () => {
    const body = 'Some text\n<Diagram id="d1" type="mermaid" />\nMore text';
    const segments = parseMdxBody(body);
    const blocks = segments.filter((s) => s.kind === "block");

    expect(blocks).toHaveLength(1);
    expect(blocks[0].tag).toBe("Diagram");
    expect(blocks[0].id).toBe("d1");
    expect(blocks[0].attributes.type).toBe("mermaid");
    expect(blocks[0].content).toBe("");
  });

  it("parses a self-closing <RichText /> tag with no attributes", () => {
    const body = "<RichText id=\"empty\" />";
    const segments = parseMdxBody(body);
    const blocks = segments.filter((s) => s.kind === "block");

    expect(blocks).toHaveLength(1);
    expect(blocks[0].tag).toBe("RichText");
    expect(blocks[0].content).toBe("");
  });

  it("parses a mixed body with self-closing and normal-form P1 tags", () => {
    const body = [
      "# Title",
      "",
      '<RichText id="r1">',
      "Hello **world**",
      "</RichText>",
      "",
      '<Diagram id="d1" type="mermaid" />',
      "",
      '<AnnotatedCode id="a1" language="rust">',
      "fn main() {}",
      "</AnnotatedCode>",
    ].join("\n");

    const segments = parseMdxBody(body);
    const blocks = segments.filter((s) => s.kind === "block");
    const mds = segments.filter((s) => s.kind === "markdown");

    expect(blocks.map((b) => b.tag)).toEqual([
      "RichText",
      "Diagram",
      "AnnotatedCode",
    ]);
    expect(blocks[0].content).toContain("Hello");
    expect(blocks[1].content).toBe("");
    expect(blocks[2].content).toContain("fn main");

    expect(mds.length).toBeGreaterThanOrEqual(1);
    expect(mds[0].text).toContain("# Title");
  });
});

describe("parseMdxBody — lowercase and non-PascalCase tags are NOT parsed as blocks", () => {
  it("does not extract lowercase HTML tags as blocks", () => {
    const body =
      '<div class="container">Hello</div>\n<span>world</span>\n<p>paragraph</p>';
    const segments = parseMdxBody(body);
    const blocks = segments.filter((s) => s.kind === "block");

    expect(blocks).toHaveLength(0);
    expect(segments).toHaveLength(1);
    expect(segments[0].kind).toBe("markdown");
  });

  it("does not extract kebab-case tags as blocks", () => {
    const body = "<my-component>content</my-component>";
    const segments = parseMdxBody(body);
    const blocks = segments.filter((s) => s.kind === "block");

    expect(blocks).toHaveLength(0);
  });

  it("preserves angle brackets in markdown content", () => {
    const body =
      "Use `Vec<T>` in Rust.\nAlso see `Option<None>` for generics.";
    const segments = parseMdxBody(body);

    expect(segments).toHaveLength(1);
    expect(segments[0].kind).toBe("markdown");
    expect(segments[0].text).toContain("Vec<T>");
  });
});

describe("isPascalCaseTag", () => {
  it("returns true for canonical block tags", () => {
    for (const tag of CANONICAL_V1_TAGS) {
      expect(isPascalCaseTag(tag)).toBe(true);
    }
  });

  it("returns true for single PascalCase word", () => {
    expect(isPascalCaseTag("Diagram")).toBe(true);
  });

  it("returns false for lowercase tags", () => {
    expect(isPascalCaseTag("div")).toBe(false);
    expect(isPascalCaseTag("span")).toBe(false);
    expect(isPascalCaseTag("richText")).toBe(false);
  });

  it("returns false for kebab-case tags", () => {
    expect(isPascalCaseTag("rich-text")).toBe(false);
    expect(isPascalCaseTag("my-component")).toBe(false);
  });

  it("returns false for empty string", () => {
    expect(isPascalCaseTag("")).toBe(false);
  });
});

describe("extractBlockTags", () => {
  it("extracts all PascalCase block tags from the canonical fixture", () => {
    const tags = extractBlockTags(CANONICAL_MDX);
    expect(tags).toEqual(CANONICAL_V1_TAGS);
  });

  it("extracts self-closing tags", () => {
    const body = '<Diagram id="d1" />\n<RichText id="r1">content</RichText>';
    const tags = extractBlockTags(body);
    expect(tags).toEqual(["Diagram", "RichText"]);
  });

  it("returns empty array for pure markdown body", () => {
    const body = "# Hello\n\nJust some markdown.\n\n```code```";
    const tags = extractBlockTags(body);
    expect(tags).toEqual([]);
  });

  it("returns empty array for empty string", () => {
    expect(extractBlockTags("")).toEqual([]);
  });
});

describe("extractProposalBlockIds", () => {
  it("extracts registered block ids for pre-validation", () => {
    const ids = extractProposalBlockIds(
      '<RichText id="intro">Intro</RichText>\n<Diagram id="flow" type="mermaid" />',
    );

    expect(ids).toEqual([
      { id: "intro", tag: "RichText", type: "rich-text" },
      { id: "flow", tag: "Diagram", type: "diagram" },
    ]);
  });

  it("ignores unknown PascalCase tags while extracting ids", () => {
    const ids = extractProposalBlockIds(
      '<UnknownBlock id="ignored" />\n<RichText id="kept">Kept</RichText>',
    );

    expect(ids).toEqual([{ id: "kept", tag: "RichText", type: "rich-text" }]);
  });
});
