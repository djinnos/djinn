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
});
