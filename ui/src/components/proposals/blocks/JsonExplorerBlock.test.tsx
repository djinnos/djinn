import { describe, expect, it } from "vitest";
import { fireEvent, render, screen } from "@/test/test-utils";

import { JsonExplorerBlock, JsonTree } from "./JsonExplorerBlock";

describe("JsonExplorerBlock", () => {
  it("renders a typed tree with keys, type-colored values, and counts", () => {
    render(
      <JsonExplorerBlock id="j1" attributes={{ title: "Sample" }}>
        {'{ "id": "abc", "active": true, "tags": ["a", "b"] }'}
      </JsonExplorerBlock>,
    );
    // Header label (shell + tree toolbar both say JSON) + title.
    expect(screen.getAllByText("JSON").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("Sample")).toBeInTheDocument();
    // Keys render (quoted).
    expect(screen.getByText('"id"')).toBeInTheDocument();
    expect(screen.getByText('"active"')).toBeInTheDocument();
    // A string and a boolean leaf render.
    expect(screen.getByText('"abc"')).toBeInTheDocument();
    expect(screen.getByText("true")).toBeInTheDocument();
  });

  it("collapses a nested container and shows its summary on toggle", () => {
    render(
      <JsonExplorerBlock id="j2" attributes={{}}>
        {'{ "outer": { "a": 1, "b": 2, "c": 3 } }'}
      </JsonExplorerBlock>,
    );
    // The nested object key is present (root expanded by default).
    const toggle = screen.getByText('"outer"').closest("button");
    expect(toggle).not.toBeNull();
    // Collapse the nested object: its key-count summary appears.
    fireEvent.click(toggle!);
    expect(screen.getByText("3 keys")).toBeInTheDocument();
  });

  it("exposes expand-all / collapse-all controls for containers", () => {
    render(
      <JsonExplorerBlock id="j3" attributes={{}}>
        {'{ "a": { "b": 1 } }'}
      </JsonExplorerBlock>,
    );
    expect(
      screen.getByRole("button", { name: "Expand all" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Collapse all" }),
    ).toBeInTheDocument();
  });

  it("falls back to a styled <pre> + error when children are not valid JSON", () => {
    render(
      <JsonExplorerBlock id="j4" attributes={{}}>
        {"not json at all { oops"}
      </JsonExplorerBlock>,
    );
    expect(screen.getByText("not json at all { oops")).toBeInTheDocument();
    expect(screen.getByText(/Could not parse JSON/)).toBeInTheDocument();
    // No tree actions in the fallback.
    expect(screen.queryByRole("button", { name: "Expand all" })).toBeNull();
  });
});

describe("JsonTree (reusable surface)", () => {
  it("renders a custom label and a tree for parseable JSON", () => {
    render(<JsonTree code={'{ "k": 1 }'} label="Example" />);
    expect(screen.getByText("Example")).toBeInTheDocument();
    expect(screen.getByText('"k"')).toBeInTheDocument();
  });

  it("falls back to a <pre> for non-JSON examples", () => {
    render(<JsonTree code={"plain text body"} label="Example" />);
    expect(screen.getByText("plain text body")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Expand all" })).toBeNull();
  });
});
