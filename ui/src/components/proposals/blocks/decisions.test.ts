import { describe, expect, it } from "vitest";

import {
  DECISION_STATUSES,
  parseDecisions,
  type DecisionStatus,
} from "./decisions";

describe("parseDecisions", () => {
  it("returns [] for empty / whitespace input (caller falls back)", () => {
    expect(parseDecisions("")).toEqual([]);
    expect(parseDecisions("   \n  \n")).toEqual([]);
  });

  it("returns [] when the body has no headings (fully-unparseable → fallback)", () => {
    // Freeform prose with no `##`/`###` heading is not a decision card.
    const result = parseDecisions(
      "We chose JWT over session cookies for stateless auth.",
    );
    expect(result).toEqual([]);
  });

  it("parses each declared ADR status token into a badge status", () => {
    for (const token of DECISION_STATUSES) {
      const result = parseDecisions(`### Some Decision\nStatus: ${token}\n`);
      expect(result).toHaveLength(1);
      expect(result[0]!.status).toBe(token);
      expect(result[0]!.title).toBe("Some Decision");
    }
  });

  it("does NOT infer status from prose (no declared token ⇒ no status)", () => {
    // Body says 'we accepted' but no `Status:` line — must NOT become accepted.
    const result = parseDecisions(
      "### Use JWT\nWe accepted this after weighing the rejected alternatives.\n",
    );
    expect(result).toHaveLength(1);
    expect(result[0]!.status).toBeUndefined();
    expect(result[0]!.body).toContain("We accepted this");
  });

  it("ignores an invalid declared token (still no status, line consumed)", () => {
    const result = parseDecisions("### Decision\nStatus: maybe-later\n");
    expect(result).toHaveLength(1);
    expect(result[0]!.status).toBeUndefined();
    // The bogus status line must not leak into the rendered body.
    expect(result[0]!.body ?? "").not.toContain("maybe-later");
  });

  it("reads a declared status from an inline [token] heading marker", () => {
    const result = parseDecisions("### Use a queue [accepted]\nSome body.\n");
    expect(result).toHaveLength(1);
    expect(result[0]!.status).toBe("accepted");
    expect(result[0]!.title).toBe("Use a queue");
  });

  it("lets an explicit Status: line override the inline heading marker", () => {
    const result = parseDecisions(
      "### Use a queue [proposed]\nStatus: accepted\nbody\n",
    );
    expect(result[0]!.status).toBe("accepted");
  });

  it("leaves a real bracketed title word alone (not a valid token)", () => {
    const result = parseDecisions("### Adopt [internal] tooling\nbody\n");
    expect(result[0]!.title).toBe("Adopt [internal] tooling");
    expect(result[0]!.status).toBeUndefined();
  });

  it("parses ADR Context / Decision / Consequences sub-sections", () => {
    const md = [
      "### Use JWT for stateless auth",
      "Status: accepted",
      "",
      "Context",
      "We scale horizontally without sticky sessions.",
      "",
      "Decision",
      "Adopt short-lived JWTs validated at the edge.",
      "",
      "Consequences",
      "No central session store; revocation needs a denylist.",
    ].join("\n");
    const result = parseDecisions(md);
    expect(result).toHaveLength(1);
    const d = result[0]!;
    expect(d.status).toBe("accepted");
    expect(d.sections?.context).toContain("horizontally");
    expect(d.sections?.decision).toContain("short-lived JWTs");
    expect(d.sections?.consequences).toContain("denylist");
    expect(d.body).toBeUndefined();
  });

  it("parses sub-section labels written as markdown headings / colon form", () => {
    const md = [
      "## Pick Postgres",
      "Status: accepted",
      "#### Context:",
      "We need transactions.",
      "**Decision**",
      "Use Postgres 16.",
    ].join("\n");
    const result = parseDecisions(md);
    expect(result[0]!.sections?.context).toContain("transactions");
    expect(result[0]!.sections?.decision).toContain("Postgres 16");
  });

  it("keeps a freeform body when there are no ADR sub-sections", () => {
    const result = parseDecisions(
      "### Use REST not GraphQL\nStatus: accepted\n\nSimpler for our small surface area.\n",
    );
    expect(result[0]!.sections).toBeUndefined();
    expect(result[0]!.body).toContain("Simpler for our small surface area.");
  });

  it("surfaces 'superseded by …' target text", () => {
    const result = parseDecisions(
      "### Old token scheme\nStatus: superseded by #3\nLegacy approach.\n",
    );
    expect(result[0]!.status).toBe("superseded");
    expect(result[0]!.supersededBy).toBe("#3");
  });

  it("handles a superseded status with no 'by' target", () => {
    const result = parseDecisions("### Old\nStatus: superseded\n");
    expect(result[0]!.status).toBe("superseded");
    expect(result[0]!.supersededBy).toBeUndefined();
  });

  it("parses a mixed multi-decision block", () => {
    const md = [
      "### Use JWT",
      "Status: accepted",
      "Decision",
      "Short-lived JWTs.",
      "",
      "### Sticky sessions",
      "Status: rejected",
      "Operationally fragile.",
      "",
      "### Edge caching",
      "No status declared here, just prose.",
    ].join("\n");
    const result = parseDecisions(md);
    expect(result).toHaveLength(3);

    expect(result[0]!.title).toBe("Use JWT");
    expect(result[0]!.status).toBe("accepted");
    expect(result[0]!.sections?.decision).toContain("Short-lived");

    expect(result[1]!.title).toBe("Sticky sessions");
    expect(result[1]!.status).toBe("rejected");
    expect(result[1]!.body).toContain("Operationally fragile.");

    expect(result[2]!.title).toBe("Edge caching");
    expect(result[2]!.status).toBeUndefined();
    expect(result[2]!.body).toContain("No status declared");
  });

  it("tolerates a **Status:** line with bold and bracketed value", () => {
    const result = parseDecisions("### D\n**Status:** [deprecated]\n");
    expect(result[0]!.status).toBe("deprecated");
  });

  it("status is case-insensitive on the declared token", () => {
    const result = parseDecisions("### D\nStatus: Accepted\n");
    expect(result[0]!.status).toBe("accepted");
  });

  it("exposes the canonical ADR status vocabulary", () => {
    expect(DECISION_STATUSES).toEqual([
      "proposed",
      "accepted",
      "rejected",
      "deprecated",
      "superseded",
    ] satisfies DecisionStatus[]);
  });
});
