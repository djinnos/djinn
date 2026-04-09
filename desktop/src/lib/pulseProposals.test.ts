import { describe, expect, it } from "vitest";
import { parseProposalItems } from "@/lib/pulseProposals";

describe("parseProposalItems", () => {
  it("threads proposal mtime into modifiedAt", () => {
    const items = parseProposalItems({
      items: [
        {
          id: "adr-123",
          title: "Draft",
          path: "/tmp/adr-123.md",
          mtime: "2026-04-09T10:11:12Z",
        },
      ],
    });

    expect(items[0]?.modifiedAt).toBe("2026-04-09T10:11:12Z");
  });

  it("falls back to null when proposal mtime is missing", () => {
    const items = parseProposalItems({
      items: [
        {
          id: "adr-124",
          title: "Draft",
          path: "/tmp/adr-124.md",
        },
      ],
    });

    expect(items[0]?.modifiedAt).toBeNull();
  });
});
