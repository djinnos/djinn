import { describe, expect, it } from "vitest";

import { parseWorkspacesResponse } from "./codeGraph";

describe("parseWorkspacesResponse", () => {
  it("normalizes workspaces from the tagged MCP response", () => {
    expect(
      parseWorkspacesResponse({
        workspaces: [
          {
            slug: "api",
            display_name: "API",
            root_path: "server/api",
            language: "rust",
            status: "warm",
          },
        ],
      }),
    ).toEqual([
      {
        slug: "api",
        display: "API",
        root: "server/api",
        language: "rust",
        status: "warm",
      },
    ]);
  });

  it("accepts alternate wire field names and trims non-empty strings", () => {
    expect(
      parseWorkspacesResponse([
        {
          workspace_slug: " web ",
          label: " Web ",
          path: " ui ",
          indexer: "typescript",
          warm_status: "pending",
        },
      ]),
    ).toEqual([
      {
        slug: "web",
        display: "Web",
        root: "ui",
        language: "typescript",
        status: "pending",
      },
    ]);
  });

  it("drops entries without a non-empty slug", () => {
    expect(
      parseWorkspacesResponse({
        workspaces: [
          { slug: "" },
          { workspace_slug: "   " },
          { slug: 42 },
          { slug: "api" },
        ],
      }),
    ).toEqual([
      {
        slug: "api",
        display: undefined,
        root: undefined,
        language: undefined,
        status: undefined,
      },
    ]);
  });
});
