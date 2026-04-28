import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { render, screen, waitFor } from "@/test/test-utils";
import userEvent from "@testing-library/user-event";

import { CitationLink } from "./CitationLink";
import { useCodeGraphStore } from "@/stores/codeGraphStore";
import { projectStore } from "@/stores/projectStore";
import type { ParsedCitation } from "@/lib/citationParser";

// `useNavigate` returns a function we can spy on. Vitest mocks the
// module identifier; the rest of react-router-dom (MemoryRouter etc.)
// is preserved via `importActual`.
const navigateMock = vi.fn();
vi.mock("react-router-dom", async () => {
  const actual = await vi.importActual<typeof import("react-router-dom")>(
    "react-router-dom",
  );
  return {
    ...actual,
    useNavigate: () => navigateMock,
  };
});

// Stub the MCP search call so we don't need a server.
const searchMock = vi.fn();
vi.mock("@/api/codeGraph", async () => {
  const actual = await vi.importActual<typeof import("@/api/codeGraph")>(
    "@/api/codeGraph",
  );
  return {
    ...actual,
    searchSymbols: (...args: unknown[]) => searchMock(...args),
  };
});

const fileCite: ParsedCitation = {
  kind: "file",
  raw: "[[file:src/auth.rs:10-20]]",
  path: "src/auth.rs",
  startLine: 10,
  endLine: 20,
};

const symbolCite: ParsedCitation = {
  kind: "symbol",
  raw: "[[symbol:Function:check_permission]]",
  symbolKind: "Function",
  name: "check_permission",
};

beforeEach(() => {
  navigateMock.mockClear();
  searchMock.mockReset();
  useCodeGraphStore.setState({
    selectionId: null,
    citationIds: new Set(),
  });
  // Pretend a project is selected so symbol citations resolve.
  projectStore.setState({
    selectedProjectId: "proj-1",
  });
});

afterEach(() => {
  useCodeGraphStore.setState({ selectionId: null, citationIds: new Set() });
});

describe("CitationLink — file form", () => {
  it("renders a clickable label", () => {
    render(<CitationLink citation={fileCite} />);
    expect(
      screen.getByRole("button", { name: /open src\/auth\.rs:10-20/i }),
    ).toBeInTheDocument();
  });

  it("pins the file node id and navigates on click", async () => {
    const user = userEvent.setup();
    render(<CitationLink citation={fileCite} />);
    await user.click(screen.getByRole("button"));
    await waitFor(() => {
      expect(useCodeGraphStore.getState().citationIds.has("file:src/auth.rs"))
        .toBe(true);
    });
    expect(useCodeGraphStore.getState().selectionId).toBe("file:src/auth.rs");
    expect(navigateMock).toHaveBeenCalledWith("/code-graph");
    expect(searchMock).not.toHaveBeenCalled();
  });
});

describe("CitationLink — symbol form, single high-confidence hit", () => {
  it("pins the matching node id without showing a popover", async () => {
    searchMock.mockResolvedValue({
      hits: [
        {
          key: "symbol:check_permission",
          kind: "function",
          display_name: "check_permission",
          score: 0.97,
          file: "src/auth.rs",
        },
      ],
    });

    const user = userEvent.setup();
    render(<CitationLink citation={symbolCite} />);
    await user.click(screen.getByRole("button", { name: /Function:check_permission/i }));

    await waitFor(() => {
      expect(useCodeGraphStore.getState().citationIds.has("symbol:check_permission"))
        .toBe(true);
    });
    expect(navigateMock).toHaveBeenCalledWith("/code-graph");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });
});

describe("CitationLink — symbol form, ambiguous", () => {
  it("renders a candidate popover when >1 hit", async () => {
    searchMock.mockResolvedValue({
      hits: [
        { key: "symbol:a", kind: "function", display_name: "a", score: 0.9, file: "src/a.rs" },
        { key: "symbol:b", kind: "function", display_name: "b", score: 0.6, file: "src/b.rs" },
      ],
    });

    const user = userEvent.setup();
    render(<CitationLink citation={symbolCite} />);
    await user.click(screen.getByRole("button", { name: /Function:check_permission/i }));

    expect(await screen.findByRole("dialog", { name: /candidates/i })).toBeInTheDocument();
    // Confirm both hits show up in the popover.
    expect(screen.getByText("a")).toBeInTheDocument();
    expect(screen.getByText("b")).toBeInTheDocument();
    expect(navigateMock).not.toHaveBeenCalled();
  });

  it("clicking a candidate pins it and navigates", async () => {
    searchMock.mockResolvedValue({
      hits: [
        { key: "symbol:a", kind: "function", display_name: "a", score: 0.9, file: "src/a.rs" },
        { key: "symbol:b", kind: "function", display_name: "b", score: 0.6, file: "src/b.rs" },
      ],
    });

    const user = userEvent.setup();
    render(<CitationLink citation={symbolCite} />);
    await user.click(screen.getByRole("button", { name: /Function:check_permission/i }));

    const candidate = await screen.findByText("b");
    await user.click(candidate);

    await waitFor(() => {
      expect(useCodeGraphStore.getState().citationIds.has("symbol:b")).toBe(true);
    });
    expect(navigateMock).toHaveBeenCalledWith("/code-graph");
  });
});
