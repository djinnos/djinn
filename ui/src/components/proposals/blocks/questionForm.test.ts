import { describe, expect, it } from "vitest";

import { parseQuestions } from "./questionForm";

describe("parseQuestions", () => {
  it("returns [] for empty / whitespace input (fallback signal)", () => {
    expect(parseQuestions("")).toEqual([]);
    expect(parseQuestions("   \n  \n")).toEqual([]);
  });

  it("treats a single `?`-terminated line as one question", () => {
    const qs = parseQuestions("Should we use Redis or Memcached for caching?");
    expect(qs).toHaveLength(1);
    expect(qs[0]!.question).toBe(
      "Should we use Redis or Memcached for caching?",
    );
    expect(qs[0]!.detail).toEqual([]);
    expect(qs[0]!.recommended).toBe(false);
  });

  it("splits numbered lines into separate questions", () => {
    const body = [
      "1. Which database should we use?",
      "2) Do we need read replicas?",
      "(3) Should auth be session-based?",
    ].join("\n");
    const qs = parseQuestions(body);
    expect(qs.map((q) => q.question)).toEqual([
      "Which database should we use?",
      "Do we need read replicas?",
      "Should auth be session-based?",
    ]);
  });

  it("splits `?`-terminated sentences into separate questions", () => {
    const body = [
      "Should we cache aggressively?",
      "What is the eviction policy?",
    ].join("\n");
    const qs = parseQuestions(body);
    expect(qs).toHaveLength(2);
    expect(qs[1]!.question).toBe("What is the eviction policy?");
  });

  it("starts a new question on a bold line and a heading", () => {
    const body = [
      "**Caching strategy**",
      "Some context about caching.",
      "### Auth model",
      "Some context about auth.",
    ].join("\n");
    const qs = parseQuestions(body);
    expect(qs.map((q) => q.question)).toEqual([
      "Caching strategy",
      "Auth model",
    ]);
    expect(qs[0]!.detail).toEqual(["Some context about caching."]);
    expect(qs[1]!.detail).toEqual(["Some context about auth."]);
  });

  it("groups following plain lines and bullets as sub-detail", () => {
    const body = [
      "1. Which caching layer?",
      "We expect ~10k req/s at peak.",
      "- Redis is the team default",
      "- Memcached is simpler to operate",
      "2. Do we need replicas?",
      "Only if read load grows.",
    ].join("\n");
    const qs = parseQuestions(body);
    expect(qs).toHaveLength(2);
    expect(qs[0]!.question).toBe("Which caching layer?");
    expect(qs[0]!.detail).toEqual([
      "We expect ~10k req/s at peak.",
      "Redis is the team default",
      "Memcached is simpler to operate",
    ]);
    expect(qs[1]!.detail).toEqual(["Only if read load grows."]);
  });

  it("flags items tagged (recommended) and strips the tag", () => {
    const body = [
      "1. Use Redis (recommended)",
      "2. Use Memcached",
    ].join("\n");
    const qs = parseQuestions(body);
    expect(qs[0]!.question).toBe("Use Redis");
    expect(qs[0]!.recommended).toBe(true);
    expect(qs[1]!.recommended).toBe(false);
  });

  it("attaches pre-header leading detail to the first question", () => {
    const body = [
      "Here are the open items to resolve before build:",
      "1. Pick a database?",
    ].join("\n");
    const qs = parseQuestions(body);
    expect(qs).toHaveLength(1);
    expect(qs[0]!.question).toBe("Pick a database?");
    expect(qs[0]!.detail).toEqual([
      "Here are the open items to resolve before build:",
    ]);
  });

  it("falls back to a single question when no header is recognised", () => {
    const body = "We still need to decide on the deployment region.";
    const qs = parseQuestions(body);
    expect(qs).toHaveLength(1);
    expect(qs[0]!.question).toBe(body);
    expect(qs[0]!.detail).toEqual([]);
  });

  it("keeps multi-line no-header prose as one question with detail", () => {
    const body = ["First consideration here.", "Second consideration here."].join(
      "\n",
    );
    const qs = parseQuestions(body);
    expect(qs).toHaveLength(1);
    expect(qs[0]!.question).toBe("First consideration here.");
    expect(qs[0]!.detail).toEqual(["Second consideration here."]);
  });

  it("makes each top-level `?`-bullet its own question (flat list)", () => {
    const body = [
      "- Which database should we use?",
      "- Do we need read replicas?",
      "- Should auth be session-based?",
      "- Where does the cache live?",
    ].join("\n");
    const qs = parseQuestions(body);
    expect(qs.map((q) => q.question)).toEqual([
      "Which database should we use?",
      "Do we need read replicas?",
      "Should auth be session-based?",
      "Where does the cache live?",
    ]);
    for (const q of qs) expect(q.detail).toEqual([]);
  });

  it("treats indented sub-bullets under a top-level question-bullet as detail", () => {
    const body = [
      "- Which caching layer should we pick?",
      "  - Redis is the team default",
      "  - Memcached is simpler to operate?",
      "- Do we need replicas?",
    ].join("\n");
    const qs = parseQuestions(body);
    expect(qs).toHaveLength(2);
    expect(qs[0]!.question).toBe("Which caching layer should we pick?");
    expect(qs[0]!.detail).toEqual([
      "Redis is the team default",
      "Memcached is simpler to operate?",
    ]);
    expect(qs[1]!.question).toBe("Do we need replicas?");
    expect(qs[1]!.detail).toEqual([]);
  });

  it("keeps a `?`-bullet nested under a numbered question as detail", () => {
    const body = [
      "1. Which database should we use?",
      "- Is Postgres acceptable?",
      "- Do we need replicas?",
    ].join("\n");
    const qs = parseQuestions(body);
    expect(qs).toHaveLength(1);
    expect(qs[0]!.question).toBe("Which database should we use?");
    expect(qs[0]!.detail).toEqual([
      "Is Postgres acceptable?",
      "Do we need replicas?",
    ]);
  });

  it("keeps a `?`-bullet nested under a heading question as detail", () => {
    const body = [
      "### Auth model",
      "- Should we use sessions?",
      "- Should we use JWTs?",
    ].join("\n");
    const qs = parseQuestions(body);
    expect(qs).toHaveLength(1);
    expect(qs[0]!.question).toBe("Auth model");
    expect(qs[0]!.detail).toEqual([
      "Should we use sessions?",
      "Should we use JWTs?",
    ]);
  });

  it("reads a uniformly-indented `?`-bullet list as separate questions", () => {
    const body = [
      "  - First open question here?",
      "  - Second open question here?",
    ].join("\n");
    const qs = parseQuestions(body);
    expect(qs.map((q) => q.question)).toEqual([
      "First open question here?",
      "Second open question here?",
    ]);
    for (const q of qs) expect(q.detail).toEqual([]);
  });

  it("parses the r0io proposal's 6 open-question bullets as 6 questions", () => {
    // Verbatim <QuestionForm> children from proposal r0io (the real bug case).
    const body = [
      "- Does the **architect** role also get the `visual-spec` native skill by default, or only the planner?",
      "- How is the native skill **versioned across deploys** — a version stamp recorded in the proposal's revision trail, and what happens when a proposal authored under v1 is re-refined under v2?",
      "- Should `get_block_catalog` also return the **canonical example body** (the `canonicalProposal.mdx` fixture), or only tags/fields — to keep the pull payload lean?",
      "- Is **bare-`<` / `>` parser-hardening** in scope here (so authors need not backtick generics), or is it a separate parser proposal?",
      "- Should there be a small **canonical section-skeleton** (Thesis / Problem / Objective / Decisions / Risks / Open-Questions) that the enricher targets, or does the skill stay structure-agnostic?",
      "- Does the **immutability guarantee** need an enforcement test (assert native skills never appear in `agent_update`'s editable set), and where does that test live?",
    ].join("\n");
    const qs = parseQuestions(body);
    expect(qs).toHaveLength(6);
    for (const q of qs) expect(q.detail).toEqual([]);
    expect(qs[0]!.question).toContain("architect");
    expect(qs[5]!.question).toContain("immutability guarantee");
  });
});
